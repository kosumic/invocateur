use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, Duration as TimeDelta, SecondsFormat, Utc};
use pulldown_cmark::{Event, Parser, Tag, TagEnd};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

mod multiplexer;
use multiplexer::Multiplexer;

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Config {
    multiplexer: Multiplexer,
    workspace_name: String,
    tab_name: String,
    spells: Vec<String>,
    agent: String,
    agent_args: Vec<String>,
    acknowledgement_message: String,
    repos_directory: PathBuf,
    repositories: BTreeMap<String, PathBuf>,
    discover_repositories: bool,
    discover_github_repositories: bool,
    clone_missing: bool,
    poll_interval_seconds: u64,
    initial_lookback_seconds: u32,
    review_comments: bool,
    herdr_session: String,
    tmux_session: String,
    zellij_session: String,
    github_host: String,
    gh_path: String,
    herdr_path: String,
    tmux_path: String,
    zellij_path: String,
    git_path: String,
    command_timeout_seconds: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            multiplexer: Multiplexer::Herdr,
            workspace_name: "{repository}#{issue_number} {agent_name}".into(),
            tab_name: "{agent_name}".into(),
            spells: vec!["summon".into()],
            agent: "codex".into(),
            agent_args: vec![],
            acknowledgement_message:
                "🪄 Invocateur started {agent} for this task.\n\nTriggered by {comment_url}.".into(),
            repos_directory: "~/repos".into(),
            repositories: BTreeMap::new(),
            discover_repositories: true,
            discover_github_repositories: true,
            clone_missing: true,
            poll_interval_seconds: 60,
            initial_lookback_seconds: 0,
            review_comments: true,
            herdr_session: "default".into(),
            tmux_session: "invocateur".into(),
            zellij_session: "invocateur".into(),
            github_host: "github.com".into(),
            gh_path: "gh".into(),
            herdr_path: "herdr".into(),
            tmux_path: "tmux".into(),
            zellij_path: "zellij".into(),
            git_path: "git".into(),
            command_timeout_seconds: 90,
        }
    }
}

impl Config {
    fn load(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path)
            .with_context(|| format!("cannot read config {}", path.display()))?;
        let mut cfg: Self = toml::from_str(&text)
            .with_context(|| format!("invalid TOML config {}", path.display()))?;
        ensure!(!cfg.spells.is_empty(), "configure at least one spell");
        for template in [&cfg.workspace_name, &cfg.tab_name] {
            ensure!(
                !template.trim().is_empty() && !template.chars().any(char::is_control),
                "workspace_name and tab_name must be nonempty single-line templates"
            );
        }
        ensure!(
            !cfg.acknowledgement_message.trim().is_empty()
                && cfg.acknowledgement_message.len() <= 60_000,
            "acknowledgement_message must be nonempty and at most 60,000 UTF-8 bytes"
        );
        for spell in &cfg.spells {
            ensure!(
                !spell.trim().is_empty() && spell.trim() == spell && !spell.contains(['\r', '\n']),
                "spells must be nonempty, single lines without surrounding whitespace"
            );
        }
        ensure!(
            cfg.poll_interval_seconds > 0,
            "poll interval must be positive"
        );
        ensure!(
            cfg.command_timeout_seconds > 0,
            "command timeout must be positive"
        );
        ensure!(
            !cfg.agent.is_empty() && !cfg.herdr_session.is_empty(),
            "agent and Herdr session cannot be empty"
        );
        for session in [&cfg.tmux_session, &cfg.zellij_session] {
            ensure!(
                !session.is_empty()
                    && session
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte)),
                "tmux_session and zellij_session must use letters, digits, underscores, or hyphens"
            );
        }
        ensure!(
            !cfg.github_host.is_empty()
                && cfg
                    .github_host
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b".-".contains(&b)),
            "invalid GitHub hostname"
        );
        cfg.repos_directory = expand_home(&cfg.repos_directory)?;
        for (repo, path) in &mut cfg.repositories {
            ensure!(
                valid_repo(repo),
                "invalid repository {repo}: expected owner/name"
            );
            *path = expand_home(path)?;
        }
        Ok(cfg)
    }
}

fn expand_home(path: &Path) -> Result<PathBuf> {
    let value = path.to_string_lossy();
    let expanded = if value == "~" || value.starts_with("~/") {
        PathBuf::from(env::var("HOME").context("HOME is not set")?)
            .join(value.strip_prefix("~/").unwrap_or(""))
    } else {
        path.to_owned()
    };
    ensure!(
        expanded.is_absolute(),
        "path must be absolute or start with ~/: {}",
        path.display()
    );
    Ok(expanded)
}

fn valid_repo(repo: &str) -> bool {
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

// Capture both streams concurrently: a full stdout pipe must not deadlock a paginated gh request.
fn run(program: &str, args: &[String], timeout: u64) -> Result<String> {
    let mut child = Command::new(program)
        .args(args)
        .env("GH_PROMPT_DISABLED", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("HERDR_SOCKET_PATH")
        .env_remove("HERDR_SESSION")
        .env_remove("HERDR_WORKSPACE_ID")
        .env_remove("HERDR_TAB_ID")
        .env_remove("HERDR_PANE_ID")
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .env_remove("ZELLIJ")
        .env_remove("ZELLIJ_SESSION_NAME")
        .env_remove("ZELLIJ_PANE_ID")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("could not start {program}"))?;
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let out = thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).map(|_| bytes)
    });
    let err = thread::spawn(move || {
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).map(|_| bytes)
    });
    let deadline = Instant::now() + Duration::from_secs(timeout);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("{program} timed out after {timeout}s; a mutation may already have applied");
        }
        thread::sleep(Duration::from_millis(25));
    };
    let stdout = out
        .join()
        .map_err(|_| anyhow::anyhow!("stdout reader panicked"))??;
    let stderr = err
        .join()
        .map_err(|_| anyhow::anyhow!("stderr reader panicked"))??;
    ensure!(
        status.success(),
        "{program} failed ({status}): {}",
        String::from_utf8_lossy(&stderr)
    );
    Ok(String::from_utf8(stdout)?)
}

fn args(values: &[&str]) -> Vec<String> {
    values.iter().map(|s| (*s).into()).collect()
}

fn render_template(template: &str, values: &[(&str, &str)]) -> String {
    template
        .split_inclusive('}')
        .map(|part| {
            for (placeholder, value) in values {
                if let Some(prefix) = part.strip_suffix(placeholder) {
                    return format!("{prefix}{value}");
                }
            }
            part.to_owned()
        })
        .collect()
}

struct Client<'a>(&'a Config);
impl Client<'_> {
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
    fn herdr(&self, argv: &[String]) -> Result<Value> {
        let mut command = args(&["--session", &self.0.herdr_session]);
        command.extend_from_slice(argv);
        let value: Value = serde_json::from_str(&run(
            &self.0.herdr_path,
            &command,
            self.0.command_timeout_seconds,
        )?)?;
        ensure!(
            value.get("error").is_none_or(Value::is_null),
            "Herdr returned an error: {value}"
        );
        Ok(value)
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

// Match source lines only where CommonMark recognizes ordinary, unquoted text.
fn task(body: &str, spells: &[String]) -> Option<String> {
    // A custom acknowledgement can itself contain a configured spell. Never summon from it.
    if body.trim_start().starts_with("<!-- invocateur:ack:") {
        return None;
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
                        return None;
                    }
                }
            }
            Event::Text(_) if suppressed == 0 => text_ranges.push(range),
            _ => {}
        }
    }
    let mut found = false;
    let mut lines = vec![];
    let mut offset = 0;
    for raw_line in body.split_inclusive('\n') {
        let line = raw_line.trim_end_matches('\n').trim_end_matches('\r');
        let trimmed = line.trim();
        let indented = line.starts_with("    ") || line.starts_with('\t');
        let start = offset + line.len() - line.trim_start().len();
        let end = start + trimmed.len();
        if !indented
            && spells.iter().any(|spell| spell == trimmed)
            && text_ranges
                .iter()
                .any(|range| range.start <= start && end <= range.end)
        {
            found = true;
        } else {
            lines.push(line);
        }
        offset += raw_line.len();
    }
    found.then(|| lines.join("\n").trim().to_owned())
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

fn excerpt(text: &str) -> &str {
    // Bound bytes, not characters, so even Unicode-heavy prompts fit Linux's argv limit.
    let mut end = text.len().min(24_000);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
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
        let Some(instruction) = task(comment.body.as_deref().unwrap_or(""), &client.0.spells)
        else {
            continue;
        };
        let number = issue_number(&comment)?;
        let issue: Value = client.gh(&format!("repos/{repo}/issues/{number}"), false)?;
        // Readiness is checked before reserving: an unavailable server can safely be retried.
        multiplexer::ensure_ready(client.0)?;
        client.checkout(repo, directory)?;
        let name = format!("gh-{id}");
        let (workspace_name, tab_name) =
            multiplexer::names(client.0, repo, number, &name, comment.id)?;
        let prompt = format!(
            "Work on GitHub issue or pull request {repo}#{number}.\nComment: {}\n\nTitle: {}\n\nIssue/PR description:\n{}\n\nInstructions from {login}:\n{}\n\nFollow the repository's AGENTS.md instructions. Mention {repo}#{number} in any commit you make.",
            comment.html_url,
            excerpt(issue["title"].as_str().unwrap_or("")),
            excerpt(issue["body"].as_str().unwrap_or("")),
            excerpt(&instruction)
        );
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
        let result = (|| -> Result<bool> {
            if !client.claim(repo, kind, comment.id, login)? {
                return Ok(false);
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
            multiplexer::launch(client.0, &request, |pane| {
                state
                    .repositories
                    .get_mut(&key)
                    .unwrap()
                    .launches
                    .get_mut(&id)
                    .unwrap()
                    .pane = Some(pane.into());
                save(state_file, state)
            })?;
            Ok(true)
        })();
        let record = state
            .repositories
            .get_mut(&key)
            .unwrap()
            .launches
            .get_mut(&id)
            .unwrap();
        match result {
            Ok(true) => {
                record.status = "started".into();
                eprintln!("started {name} for {}", comment.html_url);
            }
            Ok(false) => {
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

fn main() -> Result<()> {
    let mut config_path = None;
    let mut state_dir = None;
    let mut once = false;
    let mut check = false;
    let mut argv = env::args().skip(1);
    while let Some(arg) = argv.next() {
        match arg.as_str() {
            "--config" => {
                config_path = Some(PathBuf::from(argv.next().context("--config needs a path")?))
            }
            "--state-dir" => {
                state_dir = Some(PathBuf::from(
                    argv.next().context("--state-dir needs a path")?,
                ))
            }
            "--once" => once = true,
            "--check-config" => check = true,
            "--help" | "-h" => {
                println!(
                    "invocateur --config FILE [--state-dir DIRECTORY] [--once | --check-config]\n\nPoll your GitHub comments and start coding agents in Herdr, tmux, or Zellij. Config format: TOML."
                );
                return Ok(());
            }
            _ => bail!("unknown argument {arg}"),
        }
    }
    let cfg = Config::load(&config_path.context("--config is required")?)?;
    if check {
        println!("configuration is valid");
        return Ok(());
    }
    let state_dir = expand_home(&state_dir.unwrap_or_else(|| {
        env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("~/.local/state"))
            .join("invocateur")
    }))?;
    fs::create_dir_all(&state_dir)?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(state_dir.join("lock"))?;
    lock.try_lock()
        .context("another invocateur instance is using this state directory")?;
    let state_file = state_dir.join("state.json");
    let mut state: State = match fs::read(&state_file) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .context("invalid state file; refusing to forget previous launches")?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => State::default(),
        Err(error) => return Err(error.into()),
    };
    let client = Client(&cfg);
    loop {
        let result = (|| -> Result<()> {
            let user: User = client.gh("user", false)?;
            let mut failed = false;
            let mut pending = BTreeSet::new();
            if cfg.discover_github_repositories {
                if let Err(error) = discover_github(&client, &mut state, &state_file, &user.login) {
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
                match poll_repo(
                    &client,
                    &mut state,
                    &state_file,
                    &user.login,
                    &repo,
                    &directory,
                ) {
                    Ok(()) => {
                        if let Some(account) =
                            state.accounts.get_mut(&account_key(&client, &user.login))
                        {
                            account
                                .pending_repositories
                                .retain(|pending| !pending.eq_ignore_ascii_case(&repo));
                        }
                        save(&state_file, &state)?;
                    }
                    Err(error) => {
                        eprintln!("{repo}: {error:#}");
                        failed = true;
                    }
                }
            }
            ensure!(!failed, "one or more repositories could not be polled");
            Ok(())
        })();
        if once {
            return result;
        }
        if let Err(error) = result {
            eprintln!("poll failed: {error:#}");
        }
        thread::sleep(Duration::from_secs(cfg.poll_interval_seconds));
    }
}
