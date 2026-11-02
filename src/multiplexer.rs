use super::{Client, Config, args, render_template, run};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    env,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Multiplexer {
    #[default]
    Herdr,
    Tmux,
    Zellij,
}

impl Multiplexer {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Herdr => "herdr",
            Self::Tmux => "tmux",
            Self::Zellij => "zellij",
        }
    }
}

pub fn session(cfg: &Config) -> &str {
    match cfg.multiplexer {
        Multiplexer::Herdr => &cfg.herdr_session,
        Multiplexer::Tmux => &cfg.tmux_session,
        Multiplexer::Zellij => &cfg.zellij_session,
    }
}

pub struct LaunchRequest<'a> {
    pub directory: &'a Path,
    pub agent_name: &'a str,
    pub prompt: &'a str,
    pub workspace_name: &'a str,
    pub tab_name: &'a str,
}

pub fn names(
    cfg: &Config,
    repo: &str,
    number: u64,
    agent_name: &str,
    comment_id: u64,
) -> Result<(String, String)> {
    let number = number.to_string();
    let comment_id = comment_id.to_string();
    let values = [
        ("{repository}", repo),
        ("{issue_number}", number.as_str()),
        ("{agent}", cfg.agent.as_str()),
        ("{agent_name}", agent_name),
        ("{comment_id}", comment_id.as_str()),
        ("{multiplexer}", cfg.multiplexer.as_str()),
        ("{session}", session(cfg)),
    ];
    let workspace = render_template(&cfg.workspace_name, &values);
    let tab = render_template(&cfg.tab_name, &values);
    for label in [&workspace, &tab] {
        ensure!(
            !label.trim().is_empty() && label.len() <= 256 && !label.chars().any(char::is_control),
            "rendered workspace and tab names must be nonempty, single-line, and at most 256 UTF-8 bytes"
        );
    }
    Ok((workspace, tab))
}

fn agent_executable(cfg: &Config) -> Result<PathBuf> {
    let executable = |path: &Path| {
        path.is_file()
            && path
                .metadata()
                .is_ok_and(|metadata| metadata.permissions().mode() & 0o111 != 0)
    };
    let path = Path::new(&cfg.agent);
    let found = if path.is_absolute() {
        executable(path).then(|| path.to_owned())
    } else if path.components().count() == 1 {
        env::split_paths(&env::var_os("PATH").unwrap_or_default())
            .map(|directory| directory.join(path))
            .find(|path| executable(path))
    } else {
        None
    };
    found.with_context(|| format!("agent executable {} is unavailable; use a program in PATH or an absolute executable path", cfg.agent))
}

pub fn ensure_ready(cfg: &Config) -> Result<()> {
    match cfg.multiplexer {
        Multiplexer::Herdr => {
            Client(cfg).herdr(&args(&["workspace", "list"]))?;
        }
        Multiplexer::Tmux => {
            agent_executable(cfg)?;
            run(
                &cfg.tmux_path,
                &args(&["has-session", "-t", &format!("={}", cfg.tmux_session)]),
                cfg.command_timeout_seconds,
            )
            .context("start the configured tmux session before using Invocateur")?;
        }
        Multiplexer::Zellij => {
            agent_executable(cfg)?;
            run(
                &cfg.zellij_path,
                &args(&[
                    "--session",
                    &cfg.zellij_session,
                    "action",
                    "query-tab-names",
                ]),
                cfg.command_timeout_seconds,
            )
            .context("start the configured Zellij session before using Invocateur")?;
        }
    }
    Ok(())
}

pub fn launch(
    cfg: &Config,
    request: &LaunchRequest<'_>,
    mut checkpoint: impl FnMut(&str) -> Result<()>,
) -> Result<()> {
    match cfg.multiplexer {
        Multiplexer::Herdr => launch_herdr(cfg, request, checkpoint),
        Multiplexer::Tmux => {
            // An explicit POSIX shell keeps this independent of the user's default shell.
            // Its complete command is one quoted tmux argument, including hostile task text.
            let quote = |value: &str| format!("'{}'", value.replace('\'', "'\\''"));
            let executable = agent_executable(cfg)?;
            let mut command = vec![quote(&executable.to_string_lossy())];
            command.extend(cfg.agent_args.iter().map(|arg| quote(arg)));
            command.push(quote(request.prompt));
            let command = format!("exec {}", command.join(" "));
            let argv = args(&[
                "new-window",
                "-d",
                "-P",
                "-F",
                "#{pane_id}",
                "-t",
                &format!("={}:", cfg.tmux_session),
                "-n",
                request.tab_name,
                "-c",
                &request.directory.to_string_lossy(),
                "--",
                "sh",
                "-c",
                &command,
            ]);
            // tmux treats a trailing semicolon as a command separator even in argv input.
            let argv: Vec<_> = argv
                .into_iter()
                .map(|arg| {
                    arg.strip_suffix(';')
                        .map(|prefix| format!("{prefix}\\;"))
                        .unwrap_or(arg)
                })
                .collect();
            let pane = run(&cfg.tmux_path, &argv, cfg.command_timeout_seconds)?;
            let pane = pane.trim();
            ensure!(
                pane.starts_with('%')
                    && pane[1..].bytes().all(|byte| byte.is_ascii_digit())
                    && pane.len() > 1,
                "tmux did not return a pane ID"
            );
            checkpoint(pane)
        }
        Multiplexer::Zellij => {
            let executable = agent_executable(cfg)?;
            let mut command = args(&[
                "--session",
                &cfg.zellij_session,
                "action",
                "new-tab",
                "--no-focus",
                "--name",
                request.tab_name,
                "--cwd",
                &request.directory.to_string_lossy(),
                "--",
                &executable.to_string_lossy(),
            ]);
            command.extend(cfg.agent_args.clone());
            command.push(request.prompt.into());
            let tab = run(&cfg.zellij_path, &command, cfg.command_timeout_seconds)?;
            let tab: u32 = tab
                .trim()
                .parse()
                .context("Zellij did not return a tab ID")?;
            checkpoint(&format!("tab:{tab}"))
        }
    }
}

fn launch_herdr(
    cfg: &Config,
    request: &LaunchRequest<'_>,
    mut checkpoint: impl FnMut(&str) -> Result<()>,
) -> Result<()> {
    let client = Client(cfg);
    let workspace = client.herdr(&args(&[
        "workspace",
        "create",
        "--cwd",
        &request.directory.to_string_lossy(),
        "--label",
        request.workspace_name,
        "--no-focus",
    ]))?;
    let pane = workspace
        .pointer("/result/root_pane/pane_id")
        .and_then(Value::as_str)
        .context("Herdr did not return a root pane ID")?;
    checkpoint(pane)?;
    let tab = workspace
        .pointer("/result/tab/tab_id")
        .and_then(Value::as_str)
        .context("Herdr did not return a root tab ID")?;
    client.herdr(&args(&["tab", "rename", tab, request.tab_name]))?;
    let mut start = args(&[
        "agent",
        "start",
        request.agent_name,
        "--kind",
        &client.0.agent,
        "--pane",
        pane,
    ]);
    if !client.0.agent_args.is_empty() {
        start.push("--".into());
        start.extend(client.0.agent_args.clone());
    }
    client.herdr(&start)?;
    client.herdr(&args(&[
        "agent",
        "prompt",
        request.agent_name,
        request.prompt,
    ]))?;
    Ok(())
}
