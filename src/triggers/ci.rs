//! Opt-in monitoring of CI on the current commit of explicitly registered branches.
use super::{
    Trigger,
    github::{Client, percent_encode, resolve_repo, save},
};
use crate::{Config, multiplexer};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

pub struct CiTrigger<'a> {
    cfg: &'a Config,
    state_file: PathBuf,
    state: State,
}

#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct State {
    incidents: BTreeMap<String, Incident>,
}

#[derive(Deserialize, Serialize)]
struct Incident {
    issue: Option<Issue>,
    status: String,
    pane: Option<String>,
    error: Option<String>,
}

#[derive(Clone, Deserialize, Serialize)]
struct Issue {
    number: u64,
    html_url: String,
}

#[derive(Deserialize)]
struct Branch {
    name: String,
    commit: Commit,
}

#[derive(Deserialize)]
struct Commit {
    sha: String,
}

#[derive(Deserialize)]
struct Runs {
    workflow_runs: Vec<WorkflowRun>,
}

#[derive(Deserialize)]
struct WorkflowRun {
    id: u64,
    workflow_id: u64,
    name: Option<String>,
    head_sha: String,
    head_branch: Option<String>,
    event: String,
    status: String,
    conclusion: Option<String>,
    html_url: String,
}

impl<'a> CiTrigger<'a> {
    pub fn new(cfg: &'a Config, state_dir: &Path) -> Result<Self> {
        let state_file = state_dir.join("ci-state.json");
        let state = match fs::read(&state_file) {
            Ok(bytes) => serde_json::from_slice(&bytes).context(
                "invalid CI state file; refusing to forget previous issues and launches",
            )?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => State::default(),
            Err(error) => return Err(error.into()),
        };
        Ok(Self {
            cfg,
            state_file,
            state,
        })
    }

    fn poll_repo(
        &mut self,
        client: &Client<'_>,
        login: &str,
        repo: &str,
        directory: &Path,
    ) -> Result<()> {
        let pages: Vec<Vec<Branch>> =
            client.gh(&format!("repos/{repo}/branches?per_page=100"), true)?;
        let mut failed = false;
        for branch in pages.into_iter().flatten() {
            if !self.cfg.ci_branches.contains(&branch.name) {
                continue;
            }
            if let Err(error) = self.poll_branch(client, login, repo, directory, &branch) {
                eprintln!("CI {repo}/{}: {error:#}", branch.name);
                failed = true;
            }
        }
        ensure!(!failed, "one or more CI branches could not be polled");
        Ok(())
    }

    fn poll_branch(
        &mut self,
        client: &Client<'_>,
        login: &str,
        repo: &str,
        directory: &Path,
        branch: &Branch,
    ) -> Result<()> {
        let sha = &branch.commit.sha;
        ensure!(
            (sha.len() == 40 || sha.len() == 64) && sha.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid branch commit SHA"
        );
        let key = format!(
            "{}/{}/{}/{}/{}",
            self.cfg.github_host,
            login.to_ascii_lowercase(),
            repo.to_ascii_lowercase(),
            percent_encode(&branch.name),
            sha
        );
        if self
            .state
            .incidents
            .get(&key)
            .is_some_and(|incident| incident.status != "issue-created")
        {
            return Ok(());
        }
        let pages: Vec<Runs> = client.gh(
            &format!(
                "repos/{repo}/actions/runs?branch={}&head_sha={sha}&per_page=100",
                percent_encode(&branch.name)
            ),
            true,
        )?;
        // A new run supersedes an older run of the same workflow and event, even
        // while queued. PR runs can share a SHA with a branch tip; ignore them.
        let mut latest = BTreeMap::new();
        for run in pages.into_iter().flat_map(|page| page.workflow_runs) {
            if run.head_sha != *sha
                || run.head_branch.as_deref() != Some(&branch.name)
                || matches!(
                    run.event.as_str(),
                    "pull_request"
                        | "pull_request_target"
                        | "pull_request_review"
                        | "pull_request_review_comment"
                        | "merge_group"
                )
            {
                continue;
            }
            let key = (run.workflow_id, run.event.clone());
            if latest
                .get(&key)
                .is_none_or(|previous: &WorkflowRun| previous.id < run.id)
            {
                latest.insert(key, run);
            }
        }
        let failures: Vec<_> = latest
            .values()
            .filter(|run| {
                run.status == "completed"
                    && matches!(
                        run.conclusion.as_deref(),
                        Some("failure" | "timed_out" | "action_required")
                    )
            })
            .collect();
        if failures.is_empty() {
            return Ok(());
        }
        // Prerequisites may be retried safely, before reserving any mutation.
        multiplexer::ensure_ready(self.cfg)?;
        client.checkout(repo, directory)?;
        if !self.state.incidents.contains_key(&key) {
            let details = failures
                .iter()
                .map(|run| {
                    format!(
                        "- {}: {} ({})",
                        run.name.as_deref().unwrap_or("Workflow"),
                        run.html_url,
                        run.conclusion.as_deref().unwrap_or("failure")
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            let body = format!(
                "<!-- invocateur:ci:{}:{sha} -->\nCI failed on the latest commit of `{}` in `{repo}`.\n\nCommit: `{sha}`\n\n{details}\n\nInvocateur will launch an agent to investigate and repair this failure.",
                percent_encode(&branch.name),
                branch.name
            );
            self.state.incidents.insert(
                key.clone(),
                Incident {
                    issue: None,
                    status: "issue-reserved".into(),
                    pane: None,
                    error: None,
                },
            );
            save(&self.state_file, &self.state)?;
            let result = (|| -> Result<Issue> {
                let title = format!("CI failure on {} ({})", branch.name, &sha[..12]);
                let issue: Issue = serde_json::from_str(&client.gh_post(
                    &format!("repos/{repo}/issues"),
                    &[("title", &title), ("body", &body)],
                    false,
                )?)?;
                ensure!(
                    issue.number > 0 && !issue.html_url.is_empty(),
                    "invalid created issue response"
                );
                Ok(issue)
            })();
            let incident = self.state.incidents.get_mut(&key).unwrap();
            match result {
                Ok(issue) => {
                    incident.issue = Some(issue);
                    incident.status = "issue-created".into();
                }
                Err(error) => {
                    incident.status = "needs-attention".into();
                    incident.error = Some(format!("{error:#}"));
                    save(&self.state_file, &self.state)?;
                    return Err(error.context(
                        "issue creation failed; inspect CI state and GitHub before retrying",
                    ));
                }
            }
            save(&self.state_file, &self.state)?;
        }
        let issue = self.state.incidents[&key]
            .issue
            .clone()
            .context("CI incident has no issue")?;
        let name = format!("ci-{}-{}", repo.replace('/', "-"), issue.number);
        let (workspace_name, tab_name) =
            multiplexer::names(self.cfg, repo, issue.number, &name, issue.number)?;
        let prompt = format!(
            "Fix the CI failure tracked by {} in {repo}, branch `{}`, commit `{sha}`.\n\nRead the issue and failed workflow logs. Check the current remote branch tip and CI before changing anything; the recorded commit may have been superseded. Reuse the checkout at {} and preserve unrelated local changes. Comment on the issue that you are working on it.\n\nIf this is a simple, well-understood fix, make the smallest change, run the relevant checks, commit directly to the affected branch and push without creating a new PR. Mention #{} in the commit message. Never force-push or bypass branch protection. After pushing, verify CI, comment on the issue with the commit and validation results, then close the issue once resolved.\n\nFor a complex or uncertain fix, use a PR referencing the issue and leave the issue open until resolved. If direct push is blocked, use a PR and explain it in the issue. If newer commits or a rerun already resolved the failure, comment with the evidence and close the issue without unnecessary changes.",
            issue.html_url,
            branch.name,
            directory.display(),
            issue.number
        );
        self.state.incidents.get_mut(&key).unwrap().status = "launch-reserved".into();
        save(&self.state_file, &self.state)?;
        let result = multiplexer::launch(
            self.cfg,
            &multiplexer::LaunchRequest {
                directory,
                agent_name: &name,
                prompt: &prompt,
                workspace_name: &workspace_name,
                tab_name: &tab_name,
            },
            |pane| {
                self.state.incidents.get_mut(&key).unwrap().pane = Some(pane.to_string());
                save(&self.state_file, &self.state)
            },
        );
        let incident = self.state.incidents.get_mut(&key).unwrap();
        match result {
            Ok(response) => {
                incident.status = "started".into();
                incident.pane = Some(response.target.to_string());
            }
            Err(error) => {
                incident.status = "needs-attention".into();
                incident.error = Some(format!("{error:#}"));
                save(&self.state_file, &self.state)?;
                return Err(error.context(
                    "CI agent launch failed; inspect Herdr and CI state before retrying",
                ));
            }
        }
        save(&self.state_file, &self.state)
    }
}

impl Trigger for CiTrigger<'_> {
    fn name(&self) -> &str {
        "ci"
    }

    fn poll(&mut self) -> Result<()> {
        let cfg = self.cfg;
        let client = Client(cfg);
        let login = client.authenticated_user()?.login;
        let catalog = client.repositories(&login)?;
        let repos: std::collections::BTreeSet<_> = cfg
            .ci_repositories
            .iter()
            .map(|repo| resolve_repo(repo, &login).to_ascii_lowercase())
            .collect();
        let mut failed = false;
        for repo in repos {
            let directory = catalog
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(&repo))
                .map(|(_, path)| path.clone())
                .unwrap_or_else(|| cfg.repos_directory.join(&repo));
            if let Err(error) = self.poll_repo(&client, &login, &repo, &directory) {
                eprintln!("CI {repo}: {error:#}");
                failed = true;
            }
        }
        ensure!(!failed, "one or more CI repositories could not be polled");
        Ok(())
    }
}
