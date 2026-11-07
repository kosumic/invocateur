use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    env,
    fs::{self, OpenOptions},
    io::Read,
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

mod api;
mod cli;
mod multiplexer;
mod triggers;
use multiplexer::Multiplexer;
use triggers::github::valid_repo_reference;

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Config {
    github_enabled: bool,
    ci_repositories: Vec<String>,
    ci_branches: Vec<String>,
    ci_poll_interval_seconds: u64,
    api_enabled: bool,
    api_listen: String,
    schedule_check_interval_seconds: u64,
    multiplexer: Multiplexer,
    workspace_name: String,
    tab_name: String,
    bot_name: String,
    use_hostname_as_bot_name: bool,
    agent: String,
    agent_args: Vec<String>,
    prefix_github_comments: bool,
    acknowledgement_message: String,
    followup_acknowledgement_message: String,
    repos_directory: PathBuf,
    repositories: BTreeMap<String, PathBuf>,
    discover_repositories: bool,
    discover_github_repositories: bool,
    clone_missing: bool,
    poll_interval_seconds: u64,
    initial_lookback_seconds: u32,
    review_comments: bool,
    herdr_session: String,
    github_host: String,
    gh_path: String,
    herdr_path: String,
    git_path: String,
    command_timeout_seconds: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            github_enabled: true,
            ci_repositories: vec![],
            ci_branches: vec!["main".into(), "master".into(), "dev".into()],
            ci_poll_interval_seconds: 60,
            api_enabled: true,
            api_listen: "127.0.0.1:7639".into(),
            schedule_check_interval_seconds: 1,
            multiplexer: Multiplexer::Herdr,
            workspace_name: "{repository}#{issue_number} {agent_name}".into(),
            tab_name: "issue-{comment_id}".into(),
            bot_name: "invo".into(),
            use_hostname_as_bot_name: false,
            agent: "codex".into(),
            agent_args: vec![],
            prefix_github_comments: true,
            acknowledgement_message:
                "🪄 [Invocateur](https://github.com/kosumic/invocateur) started {agent} for this task.\n\nTriggered by {comment_url}.".into(),
            followup_acknowledgement_message:
                "↪️ [Invocateur](https://github.com/kosumic/invocateur) forwarded this comment to {agent_name}.\n\nTriggered by {comment_url}.".into(),
            repos_directory: "~/repos".into(),
            repositories: BTreeMap::new(),
            discover_repositories: true,
            discover_github_repositories: true,
            clone_missing: true,
            poll_interval_seconds: 60,
            initial_lookback_seconds: 0,
            review_comments: true,
            herdr_session: "default".into(),
            github_host: "github.com".into(),
            gh_path: "gh".into(),
            herdr_path: "herdr".into(),
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
        for (name, message) in [
            ("acknowledgement_message", &cfg.acknowledgement_message),
            (
                "followup_acknowledgement_message",
                &cfg.followup_acknowledgement_message,
            ),
        ] {
            ensure!(
                !message.trim().is_empty() && message.len() <= 60_000,
                "{name} must be nonempty and at most 60,000 UTF-8 bytes"
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
        ensure!(
            !cfg.github_host.is_empty()
                && cfg
                    .github_host
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b".-".contains(&b)),
            "invalid GitHub hostname"
        );
        ensure!(
            cfg.ci_poll_interval_seconds > 0,
            "CI poll interval must be positive"
        );
        for repo in &cfg.ci_repositories {
            ensure!(
                valid_repo_reference(repo),
                "invalid CI repository {repo}: expected name or owner/name"
            );
        }
        ensure!(
            !cfg.ci_branches.is_empty()
                && cfg
                    .ci_branches
                    .iter()
                    .all(|branch| !branch.is_empty() && !branch.chars().any(char::is_control)),
            "ci_branches must contain nonempty branch names without control characters"
        );
        cfg.repos_directory = expand_home(&cfg.repos_directory)?;
        for (repo, path) in &mut cfg.repositories {
            ensure!(
                valid_repo_reference(repo),
                "invalid repository {repo}: expected name or owner/name"
            );
            *path = expand_home(path)?;
        }
        ensure!(
            cfg.schedule_check_interval_seconds > 0,
            "schedule check interval must be positive"
        );
        let address: std::net::SocketAddr = cfg
            .api_listen
            .parse()
            .context("api_listen must be an IP address and port")?;
        ensure!(
            address.ip().is_loopback(),
            "the local schedule API must listen on a loopback address"
        );
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
    run_in_directory(program, args, timeout, None)
}

fn run_in_directory(
    program: &str,
    args: &[String],
    timeout: u64,
    directory: Option<&Path>,
) -> Result<String> {
    run_command(program, args, timeout, directory)?.into_stdout(program)
}

struct CommandOutput {
    status: std::process::ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}
impl CommandOutput {
    fn ensure_success(&self, program: &str) -> Result<()> {
        ensure!(
            self.status.success(),
            "{program} failed ({}): {}",
            self.status,
            String::from_utf8_lossy(&self.stderr)
        );
        Ok(())
    }
    fn into_stdout(self, program: &str) -> Result<String> {
        self.ensure_success(program)?;
        String::from_utf8(self.stdout)
            .with_context(|| format!("{program} returned non-UTF-8 output"))
    }
}

fn run_command(
    program: &str,
    args: &[String],
    timeout: u64,
    directory: Option<&Path>,
) -> Result<CommandOutput> {
    let mut command = Command::new(program);
    if let Some(directory) = directory {
        command.current_dir(directory);
    }
    let mut child = command
        .process_group(0)
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
            // This process group contains only the CLI we started and its descendants.
            unsafe {
                libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
            }
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
    Ok(CommandOutput {
        status,
        stdout,
        stderr,
    })
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
            "serve" => {}
            "schedule" | "schedules" => return cli::run(argv.collect(), state_dir),
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
                    "invo serve --config FILE [--state-dir DIRECTORY] [--once | --check-config]\ninvo schedule --help\n\nRun GitHub comment and CI triggers and scheduled agent or command tasks. Config format: TOML."
                );
                return Ok(());
            }
            _ => bail!("unknown argument {arg}"),
        }
    }
    let cfg = Config::load(&config_path.context("--config is required")?)?;
    if check {
        println!("configuration is valid");
        if cfg.github_enabled {
            println!("GitHub trigger: @{}", cfg.bot_name);
        }
        println!("CI trigger: {} repositories", cfg.ci_repositories.len());
        println!(
            "Schedule API: {} ({})",
            cfg.api_listen,
            if cfg.api_enabled {
                "enabled"
            } else {
                "disabled"
            }
        );
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
    let mut active_triggers = vec![];
    if cfg.github_enabled {
        active_triggers.push(triggers::Registration::new(
            Box::new(triggers::github::GithubTrigger::new(&cfg, &state_dir)?),
            Duration::from_secs(cfg.poll_interval_seconds),
        ));
    }
    if !cfg.ci_repositories.is_empty() {
        active_triggers.push(triggers::Registration::new(
            Box::new(triggers::ci::CiTrigger::new(&cfg, &state_dir)?),
            Duration::from_secs(cfg.ci_poll_interval_seconds),
        ));
    }
    let schedule_store = std::sync::Arc::new(triggers::schedule::Store::open(&cfg, &state_dir)?);
    let _api = if cfg.api_enabled && !once {
        Some(api::Server::start(
            &cfg.api_listen,
            &state_dir,
            schedule_store.clone(),
        )?)
    } else {
        None
    };
    active_triggers.push(triggers::Registration::new(
        Box::new(triggers::schedule::ScheduleTrigger::new(
            &cfg,
            schedule_store,
        )),
        Duration::from_secs(cfg.schedule_check_interval_seconds),
    ));
    loop {
        let result = triggers::poll_all(&mut active_triggers);
        if once {
            return result;
        }
        thread::sleep(triggers::next_delay(&active_triggers));
    }
}
