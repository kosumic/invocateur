//! GitHub-specific polling, prerequisites, durable claims, and coding-agent tasks.
use super::Trigger;
use crate::{Config, args, multiplexer, multiplexer::Multiplexer, run};
use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, Duration as TimeDelta, SecondsFormat, Utc};
use pulldown_cmark::{Event, Parser, Tag, TagEnd};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::Write,
    path::{Path, PathBuf},
};

pub struct GithubTrigger<'a> {
    cfg: &'a Config,
    state_file: PathBuf,
    state: State,
}

impl<'a> GithubTrigger<'a> {
    pub fn new(cfg: &'a Config, state_dir: &Path) -> Result<Self> {
        let state_file = state_dir.join("state.json");
        let state: State = match fs::read(&state_file) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .context("invalid state file; refusing to forget previous launches")?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => State::default(),
            Err(error) => return Err(error.into()),
        };

        eprintln!(
            "GitHub trigger watching for @{} in your comments",
            cfg.bot_name
        );
        Ok(Self {
            cfg,
            state_file,
            state,
        })
    }
}

impl Trigger for GithubTrigger<'_> {
    fn name(&self) -> &str {
        "github"
    }

    fn poll(&mut self) -> Result<()> {
        let cfg = self.cfg;
        let client = Client(cfg);
        let state = &mut self.state;
        let state_file = &self.state_file;
        let user = client.authenticated_user()?;
        let mut failed = false;
        let mut pending = BTreeSet::new();
        if cfg.discover_github_repositories {
            if let Err(error) = discover_github(&client, state, state_file, &user.login) {
                eprintln!("GitHub repository discovery: {error:#}");
                failed = true;
            }
            if let Some(account) = state.accounts.get(&account_key(&client, &user.login)) {
                pending = account.pending_repositories.clone();
            }
        }
        let watched: BTreeSet<_> = cfg
            .repositories
            .keys()
            .chain(pending.iter())
            .map(|repo| repo.to_ascii_lowercase())
            .collect();
        // In global mode local checkouts are a path catalog, not an API polling list.
        // Avoid scanning the catalog altogether while there is no pending or explicit work.
        let mut repos = if cfg.discover_github_repositories && watched.is_empty() {
            BTreeMap::new()
        } else {
            client.repositories()?
        };
        if cfg.discover_github_repositories {
            for repo in &pending {
                if !repos
                    .keys()
                    .any(|existing| existing.eq_ignore_ascii_case(repo))
                {
                    repos.insert(repo.clone(), cfg.repos_directory.join(repo));
                }
            }
            repos.retain(|repo, _| watched.contains(&repo.to_ascii_lowercase()));
        }
        if repos.is_empty() {
            if cfg.discover_github_repositories {
                eprintln!("poll complete: no updated GitHub conversations");
            } else {
                eprintln!(
                    "no repositories configured or found under {}",
                    cfg.repos_directory.display()
                );
            }
        }
        for (repo, directory) in repos {
            match poll_repo(&client, state, state_file, &user.login, &repo, &directory) {
                Ok(()) => {
                    if let Some(account) =
                        state.accounts.get_mut(&account_key(&client, &user.login))
                    {
                        account
                            .pending_repositories
                            .retain(|pending| !pending.eq_ignore_ascii_case(&repo));
                    }
                    save(state_file, state)?;
                }
                Err(error) => {
                    eprintln!("{repo}: {error:#}");
                    failed = true;
                }
            }
        }
        ensure!(!failed, "one or more repositories could not be polled");
        Ok(())
    }
}

pub(crate) fn valid_repo(repo: &str) -> bool {
    let parts: Vec<_> = repo.split('/').collect();
    parts.len() == 2
        && parts.iter().all(|p| {
            !p.is_empty()
                && *p != "."
                && *p != ".."
                && p.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        })
}

fn repo_from_remote(remote: &str, host: &str) -> Option<String> {
    let https = format!("https://{host}/");
    let ssh = format!("git@{host}:");
    let ssh_url = format!("ssh://git@{host}/");
    let repo = remote
        .trim()
        .strip_prefix(&https)
        .or_else(|| remote.trim().strip_prefix(&ssh))
        .or_else(|| remote.trim().strip_prefix(&ssh_url))?
        .trim_end_matches('/')
        .trim_end_matches(".git");
    valid_repo(repo).then(|| repo.to_owned())
}

struct Client<'a>(&'a Config);
impl Client<'_> {
    // The authenticated user endpoint handles stored credentials and GH_TOKEN alike.
    // Keep prerequisite failures inside this trigger so the scheduler can run others.
    fn authenticated_user(&self) -> Result<User> {
        self.gh("user", false).map_err(|error| {
            let missing = error.chain().any(|cause| {
                cause.downcast_ref::<std::io::Error>().is_some_and(|error| {
                    error.kind() == std::io::ErrorKind::NotFound
                })
            });
            let message = if missing {
                format!("skipping poll: GitHub CLI {} is not installed or not in PATH; install gh or set gh_path", self.0.gh_path)
            } else {
                format!("skipping poll: cannot verify GitHub authentication for {}; check connectivity and run gh auth login --hostname {} if not logged in", self.0.github_host, self.0.github_host)
            };
            error.context(message)
        })
    }

    fn gh_post(
        &self,
        endpoint: &str,
        fields: &[(&str, &str)],
        include_headers: bool,
    ) -> Result<String> {
        let mut argv = args(&[
            "api",
            "--hostname",
            &self.0.github_host,
            "--method",
            "POST",
            endpoint,
        ]);
        if include_headers {
            argv.push("--include".into());
        }
        for (key, value) in fields {
            argv.extend(args(&["--raw-field", &format!("{key}={value}")]));
        }
        // Never retry POSTs: a transport failure may follow a successful GitHub mutation.
        run(&self.0.gh_path, &argv, self.0.command_timeout_seconds)
    }

    fn claim(&self, repo: &str, kind: &str, comment_id: u64, login: &str) -> Result<bool> {
        let collection = if kind == "i" { "issues" } else { "pulls" };
        let response = self.gh_post(
            &format!("repos/{repo}/{collection}/comments/{comment_id}/reactions"),
            &[("content", "eyes")],
            true,
        )?;
        let (headers, body) = response
            .split_once("\r\n\r\n")
            .or_else(|| response.split_once("\n\n"))
            .context("missing GitHub response headers")?;
        let mut status_line = headers
            .lines()
            .next()
            .context("missing GitHub HTTP status")?
            .split_whitespace();
        ensure!(
            status_line
                .next()
                .is_some_and(|part| part.starts_with("HTTP/")),
            "invalid GitHub HTTP status line"
        );
        let status: u16 = status_line
            .next()
            .context("missing GitHub HTTP status code")?
            .parse()?;
        let reaction: Value = serde_json::from_str(body)?;
        ensure!(
            reaction["content"] == "eyes"
                && reaction["user"]["login"]
                    .as_str()
                    .is_some_and(|user| user.eq_ignore_ascii_case(login)),
            "GitHub returned an unexpected claim reaction"
        );
        match status {
            201 => Ok(true),
            200 => Ok(false),
            _ => bail!("unexpected GitHub claim status {status}; refusing to launch"),
        }
    }

    fn acknowledge(&self, repo: &str, number: u64, id: &str, comment_url: &str) -> Result<String> {
        let body = format!(
            "<!-- invocateur:ack:{id} -->\n{}",
            self.0.acknowledgement_message
        );
        // Replace in one pass so placeholder-looking text inside values remains literal.
        let body = body
            .split_inclusive('}')
            .map(|part| {
                for (placeholder, value) in [
                    ("{agent}", self.0.agent.as_str()),
                    ("{comment_url}", comment_url),
                    ("{repository}", repo),
                    ("{issue_number}", &number.to_string()),
                    ("{multiplexer}", self.0.multiplexer.as_str()),
                    ("{session}", multiplexer::session(self.0)),
                ] {
                    if let Some(prefix) = part.strip_suffix(placeholder) {
                        return format!("{prefix}{value}");
                    }
                }
                part.to_owned()
            })
            .collect::<String>();
        ensure!(
            body.len() <= 64_000,
            "rendered acknowledgement exceeds 64,000 UTF-8 bytes"
        );
        let response: Value = serde_json::from_str(&self.gh_post(
            &format!("repos/{repo}/issues/{number}/comments"),
            &[("body", &body)],
            false,
        )?)?;
        response["html_url"]
            .as_str()
            .map(str::to_owned)
            .context("GitHub acknowledgement response has no URL")
    }

    fn gh<T: DeserializeOwned>(&self, endpoint: &str, paginated: bool) -> Result<T> {
        let mut argv = args(&[
            "api",
            "--hostname",
            &self.0.github_host,
            "--method",
            "GET",
            endpoint,
        ]);
        if paginated {
            argv.extend(args(&["--paginate", "--slurp"]));
        }
        let output = run(&self.0.gh_path, &argv, self.0.command_timeout_seconds)?;
        serde_json::from_str(&output).with_context(|| format!("invalid gh response for {endpoint}"))
    }
    fn remote(&self, directory: &Path) -> Result<String> {
        let output = run(
            &self.0.git_path,
            &args(&[
                "-C",
                &directory.to_string_lossy(),
                "remote",
                "get-url",
                "origin",
            ]),
            self.0.command_timeout_seconds,
        )?;
        repo_from_remote(&output, &self.0.github_host)
            .context("origin is not a supported GitHub remote")
    }
    fn repositories(&self) -> Result<BTreeMap<String, PathBuf>> {
        let mut repos = BTreeMap::new();
        if self.0.discover_repositories && self.0.repos_directory.is_dir() {
            // Support both ~/repos/name and ~/repos/owner/name, including worktrees (.git files).
            let mut candidates = vec![];
            for entry in fs::read_dir(&self.0.repos_directory)? {
                let path = entry?.path();
                if !path.is_dir() {
                    continue;
                }
                if path.join(".git").exists() {
                    candidates.push(path);
                } else {
                    for child in fs::read_dir(path)? {
                        let child = child?.path();
                        if child.join(".git").exists() {
                            candidates.push(child);
                        }
                    }
                }
            }
            candidates.sort();
            for path in candidates {
                match self.remote(&path) {
                    Ok(repo) => {
                        repos.entry(repo).or_insert(path);
                    }
                    Err(error) => eprintln!("skipping {}: {error:#}", path.display()),
                }
            }
        }
        repos.extend(self.0.repositories.clone());
        Ok(repos)
    }
    fn checkout(&self, repo: &str, directory: &Path) -> Result<()> {
        if !directory.exists() {
            ensure!(
                self.0.clone_missing,
                "checkout {} does not exist",
                directory.display()
            );
            fs::create_dir_all(directory.parent().context("checkout has no parent")?)?;
            let repository = format!("{}/{repo}", self.0.github_host);
            run(
                &self.0.gh_path,
                &args(&[
                    "repo",
                    "clone",
                    &repository,
                    &directory.to_string_lossy(),
                    "--",
                    "--depth=1",
                ]),
                self.0.command_timeout_seconds,
            )?;
        }
        ensure!(
            directory.join(".git").exists(),
            "{} is not a checkout",
            directory.display()
        );
        ensure!(
            self.remote(directory)?.eq_ignore_ascii_case(repo),
            "origin does not match {repo} in {}",
            directory.display()
        );
        Ok(())
    }
}

#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct State {
    // Namespace by hostname and authenticated account to avoid crossing identities on config changes.
    repositories: BTreeMap<String, RepoState>,
    #[serde(default)]
    accounts: BTreeMap<String, AccountState>,
}
#[derive(Deserialize, Serialize)]
struct AccountState {
    not_before: DateTime<Utc>,
    cursor: DateTime<Utc>,
    // Retain the original on-disk key so older state files and rollbacks remain readable.
    #[serde(rename = "discovered_repositories")]
    pending_repositories: BTreeSet<String>,
}
#[derive(Deserialize, Serialize)]
struct RepoState {
    not_before: DateTime<Utc>,
    cursor: DateTime<Utc>,
    launches: BTreeMap<String, Launch>,
}
#[derive(Deserialize, Serialize)]
struct Launch {
    url: String,
    agent: String,
    status: String,
    pane: Option<String>,
    error: Option<String>,
    #[serde(default)]
    acknowledgement_url: Option<String>,
    #[serde(default)]
    acknowledgement_error: Option<String>,
    #[serde(default)]
    multiplexer: Multiplexer,
    #[serde(default)]
    session: Option<String>,
    #[serde(default)]
    workspace_name: Option<String>,
    #[serde(default)]
    tab_name: Option<String>,
}

fn save(path: &Path, state: &State) -> Result<()> {
    let temp = path.with_extension("json.tmp");
    let mut file = File::create(&temp)?;
    file.write_all(&serde_json::to_vec_pretty(state)?)?;
    file.sync_all()?;
    fs::rename(&temp, path)?;
    File::open(path.parent().unwrap())?.sync_all()?;
    Ok(())
}

#[derive(Deserialize)]
struct User {
    login: String,
}
#[derive(Deserialize)]
struct Comment {
    id: u64,
    body: Option<String>,
    user: User,
    html_url: String,
    updated_at: DateTime<Utc>,
    issue_url: Option<String>,
    pull_request_url: Option<String>,
}

#[derive(Deserialize)]
struct SearchPage {
    total_count: u64,
    incomplete_results: bool,
    items: Vec<SearchItem>,
}
#[derive(Deserialize)]
struct SearchItem {
    repository_url: String,
}

fn account_key(client: &Client<'_>, login: &str) -> String {
    format!("{}/{}", client.0.github_host, login.to_ascii_lowercase())
}

fn repo_key(client: &Client<'_>, login: &str, repo: &str) -> String {
    format!(
        "{}/{}",
        account_key(client, login),
        repo.to_ascii_lowercase()
    )
}

fn percent_encode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
                char::from(byte).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

fn discover_github(
    client: &Client<'_>,
    state: &mut State,
    state_file: &Path,
    login: &str,
) -> Result<()> {
    let key = account_key(client, login);
    let poll_started = Utc::now();
    let account = state.accounts.entry(key.clone()).or_insert_with(|| {
        let start = poll_started - TimeDelta::seconds(i64::from(client.0.initial_lookback_seconds));
        AccountState {
            not_before: start,
            cursor: start,
            pending_repositories: BTreeSet::new(),
        }
    });
    let not_before = account.not_before;
    let cursor = account.cursor;
    save(state_file, state)?;
    let since = (cursor - TimeDelta::minutes(5)).to_rfc3339_opts(SecondsFormat::Secs, true);
    let mut repositories = BTreeSet::new();
    for qualifier in ["commenter", "reviewed-by"] {
        if qualifier == "reviewed-by" && !client.0.review_comments {
            continue;
        }
        let query = percent_encode(&format!("{qualifier}:{login} updated:>={since}"));
        let pages: Vec<SearchPage> = client.gh(
            &format!("search/issues?q={query}&sort=updated&order=asc&per_page=100"),
            true,
        )?;
        for page in pages {
            ensure!(
                !page.incomplete_results && page.total_count <= 1000,
                "GitHub search was incomplete or exceeded 1,000 results; discovery cursor was not advanced"
            );
            for item in page.items {
                let repo = item
                    .repository_url
                    .rsplit_once("/repos/")
                    .context("invalid repository URL in search result")?
                    .1;
                ensure!(
                    valid_repo(repo),
                    "invalid repository in search result: {repo}"
                );
                repositories.insert(repo.to_owned());
            }
        }
    }
    // Save discovered repositories and their starting cursors together. A later failed poll
    // remains recoverable even after discovery advances or the thread leaves the search window.
    for repo in &repositories {
        state
            .repositories
            .entry(repo_key(client, login, repo))
            .or_insert_with(|| RepoState {
                not_before,
                cursor: std::cmp::max(not_before, cursor - TimeDelta::minutes(5)),
                launches: BTreeMap::new(),
            });
    }
    let account = state.accounts.get_mut(&key).unwrap();
    account.pending_repositories.extend(repositories);
    account.cursor = poll_started;
    save(state_file, state)
}

// Match mentions only where CommonMark recognizes ordinary, unquoted text.
fn has_mention(body: &str, bot_name: &str) -> bool {
    // A custom acknowledgement can itself mention the bot. Never summon from it.
    if body.trim_start().starts_with("<!-- invocateur:ack:") {
        return false;
    }
    let mut suppressed = 0;
    let mut text_ranges = vec![];
    for (event, range) in Parser::new(body).into_offset_iter() {
        match event {
            Event::Start(
                Tag::BlockQuote(_)
                | Tag::CodeBlock(_)
                | Tag::HtmlBlock
                | Tag::Image { .. }
                | Tag::Link { .. },
            ) => suppressed += 1,
            Event::End(
                TagEnd::BlockQuote(_)
                | TagEnd::CodeBlock
                | TagEnd::HtmlBlock
                | TagEnd::Image
                | TagEnd::Link,
            ) => suppressed -= 1,
            Event::Html(html) | Event::InlineHtml(html) => {
                // Raw HTML quote containers can span blank lines and several Markdown blocks.
                // Conservatively ignore the whole comment rather than misclassify their contents.
                let html = html.to_ascii_lowercase();
                for tag in ["<blockquote", "</blockquote", "<q", "</q"] {
                    if html.match_indices(tag).any(|(offset, _)| {
                        html[offset + tag.len()..]
                            .starts_with(|c: char| c.is_ascii_whitespace() || c == '>' || c == '/')
                    }) {
                        return false;
                    }
                }
            }
            Event::Text(_) if suppressed == 0 => text_ranges.push(range),
            _ => {}
        }
    }
    // Also exclude literal quoted speech in otherwise ordinary Markdown text.
    // Apostrophes inside words are not opening quotes.
    let mut quote = None;
    let mut quoted_ranges = vec![];
    for range in &text_ranges {
        for (relative, character) in body[range.clone()].char_indices() {
            let offset = range.start + relative;
            let previous = body[..offset].chars().next_back();
            let next = body[offset + character.len_utf8()..].chars().next();
            if let Some((closing, start)) = quote {
                if character == closing
                    && !(matches!(closing, '\'' | '’') && next.is_some_and(char::is_alphanumeric))
                {
                    quoted_ranges.push(start..offset + character.len_utf8());
                    quote = None;
                }
            } else if previous.is_none_or(|c| !c.is_alphanumeric()) {
                let closing = match character {
                    '"' => Some('"'),
                    '\'' => Some('\''),
                    '“' => Some('”'),
                    '‘' => Some('’'),
                    _ => None,
                };
                if let Some(closing) = closing {
                    quote = Some((closing, offset));
                }
            }
        }
    }
    if let Some((_, start)) = quote {
        quoted_ranges.push(start..body.len());
    }

    let mention = format!("@{}", bot_name.to_ascii_lowercase());
    let lower = body.to_ascii_lowercase();
    for (start, _) in lower.match_indices(&mention) {
        let end = start + mention.len();
        let before = body[..start].chars().next_back();
        let mut following = body[end..].chars();
        let after = following.next();
        let start_boundary = before.is_none_or(|c| c.is_whitespace() || "([{".contains(c));
        let end_boundary = after.is_none_or(|c| c.is_whitespace() || ")]},:;!?.".contains(c))
            && !(after == Some('.') && following.next().is_some_and(char::is_alphanumeric));
        if start_boundary
            && end_boundary
            && text_ranges
                .iter()
                .any(|range| range.start <= start && end <= range.end)
            && !quoted_ranges.iter().any(|range| range.contains(&start))
        {
            return true;
        }
    }
    false
}

fn issue_number(comment: &Comment) -> Result<u64> {
    let url = comment
        .issue_url
        .as_ref()
        .or(comment.pull_request_url.as_ref())
        .context("comment has no issue/PR URL")?;
    url.rsplit('/')
        .next()
        .context("missing issue number")?
        .parse()
        .context("invalid issue number")
}

fn poll_repo(
    client: &Client<'_>,
    state: &mut State,
    state_file: &Path,
    login: &str,
    repo: &str,
    directory: &Path,
) -> Result<()> {
    let key = repo_key(client, login, repo);
    let poll_started = Utc::now();
    state.repositories.entry(key.clone()).or_insert_with(|| {
        let start = poll_started - TimeDelta::seconds(i64::from(client.0.initial_lookback_seconds));
        RepoState {
            not_before: start,
            cursor: start,
            launches: BTreeMap::new(),
        }
    });
    save(state_file, state)?;
    // Overlap catches same-second updates and delayed GitHub visibility; IDs provide deduplication.
    let since = (state.repositories[&key].cursor - TimeDelta::minutes(5))
        .to_rfc3339_opts(SecondsFormat::Secs, true);
    let mut comments = vec![];
    for (kind, endpoint) in [("i", "issues/comments"), ("r", "pulls/comments")] {
        if kind == "r" && !client.0.review_comments {
            continue;
        }
        let pages: Vec<Vec<Comment>> = client.gh(
            &format!(
                "repos/{repo}/{endpoint}?since={since}&sort=updated&direction=asc&per_page=100"
            ),
            true,
        )?;
        comments.extend(pages.into_iter().flatten().map(|comment| (kind, comment)));
    }
    comments.sort_by_key(|(_, comment)| (comment.updated_at, comment.id));
    for (kind, comment) in comments {
        let id = format!("{kind}-{}", comment.id);
        let repo_state = &state.repositories[&key];
        if !comment.user.login.eq_ignore_ascii_case(login)
            || comment.updated_at < repo_state.not_before
            || repo_state.launches.contains_key(&id)
        {
            continue;
        }
        if !has_mention(comment.body.as_deref().unwrap_or(""), &client.0.bot_name) {
            continue;
        }
        let number = issue_number(&comment)?;
        // Readiness is checked before reserving: an unavailable server can safely be retried.
        multiplexer::ensure_ready(client.0)?;
        client.checkout(repo, directory)?;
        let name = format!("gh-{id}");
        let (workspace_name, tab_name) =
            multiplexer::names(client.0, repo, number, &name, comment.id)?;
        let prompt = format!("Work on it: {}", comment.html_url);
        state.repositories.get_mut(&key).unwrap().launches.insert(
            id.clone(),
            Launch {
                url: comment.html_url.clone(),
                agent: name.clone(),
                status: "reserved".into(),
                pane: None,
                error: None,
                acknowledgement_url: None,
                acknowledgement_error: None,
                multiplexer: client.0.multiplexer,
                session: Some(multiplexer::session(client.0).to_owned()),
                workspace_name: Some(workspace_name.clone()),
                tab_name: Some(tab_name.clone()),
            },
        );
        // Persist BEFORE mutation. A crash or ambiguous failure must never silently launch twice.
        save(state_file, state)?;
        let result = (|| -> Result<Option<multiplexer::LaunchResponse>> {
            if !client.claim(repo, kind, comment.id, login)? {
                return Ok(None);
            }
            state
                .repositories
                .get_mut(&key)
                .unwrap()
                .launches
                .get_mut(&id)
                .unwrap()
                .status = "claimed".into();
            save(state_file, state)?;
            let request = multiplexer::LaunchRequest {
                directory,
                agent_name: &name,
                prompt: &prompt,
                workspace_name: &workspace_name,
                tab_name: &tab_name,
            };
            let response = multiplexer::launch(client.0, &request, |pane| {
                state
                    .repositories
                    .get_mut(&key)
                    .unwrap()
                    .launches
                    .get_mut(&id)
                    .unwrap()
                    .pane = Some(pane.to_string());
                save(state_file, state)
            })?;
            Ok(Some(response))
        })();
        let record = state
            .repositories
            .get_mut(&key)
            .unwrap()
            .launches
            .get_mut(&id)
            .unwrap();
        match result {
            Ok(Some(response)) => {
                record.pane = Some(response.target.to_string());
                record.status = "started".into();
                eprintln!("started {name} for {}", comment.html_url);
            }
            Ok(None) => {
                record.status = "claimed-elsewhere".into();
                eprintln!("skipping {}: already claimed on GitHub", comment.html_url);
            }
            Err(error) => {
                record.status = "needs-attention".into();
                record.error = Some(format!("{error:#}"));
                eprintln!(
                    "{} needs attention: {error:#}; inspect the multiplexer and {} before retrying",
                    comment.html_url,
                    state_file.display()
                );
            }
        }
        save(state_file, state)?;
        if state.repositories[&key].launches[&id].status == "started" {
            let acknowledgement = client.acknowledge(repo, number, &id, &comment.html_url);
            let record = state
                .repositories
                .get_mut(&key)
                .unwrap()
                .launches
                .get_mut(&id)
                .unwrap();
            match acknowledgement {
                Ok(url) => record.acknowledgement_url = Some(url),
                Err(error) => {
                    record.acknowledgement_error = Some(format!("{error:#}"));
                    eprintln!(
                        "{name} is running, but its acknowledgement failed: {error:#}; not retrying an ambiguous comment POST"
                    );
                }
            }
            save(state_file, state)?;
        }
    }
    state.repositories.get_mut(&key).unwrap().cursor = poll_started;
    save(state_file, state)
}
