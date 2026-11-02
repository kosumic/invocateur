use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    env,
    fs::{self, OpenOptions},
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

mod multiplexer;
mod triggers;
use multiplexer::Multiplexer;
use triggers::github::valid_repo;

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Config {
    multiplexer: Multiplexer,
    workspace_name: String,
    tab_name: String,
    bot_name: String,
    use_hostname_as_bot_name: bool,
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
            bot_name: "invo".into(),
            use_hostname_as_bot_name: false,
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
        if cfg.use_hostname_as_bot_name {
            let hostname = hostname::get().context("cannot determine the system hostname")?;
            cfg.bot_name = hostname
                .to_str()
                .context("system hostname is not valid UTF-8")?
                .split('.')
                .next()
                .unwrap_or_default()
                .to_owned();
        }
        ensure!(
            !cfg.bot_name.is_empty()
                && cfg
                    .bot_name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b)),
            "effective bot name must contain only letters, digits, underscores, or hyphens, without @"
        );
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
        println!("configuration is valid (trigger: @{})", cfg.bot_name);
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
    let mut active_triggers: Vec<Box<dyn triggers::Trigger + '_>> = vec![Box::new(
        triggers::github::GithubTrigger::new(&cfg, &state_dir)?,
    )];
    loop {
        let result = triggers::poll_all(&mut active_triggers);
        if once {
            return result;
        }
        thread::sleep(Duration::from_secs(cfg.poll_interval_seconds));
    }
}
