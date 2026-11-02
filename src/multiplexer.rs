use super::{Config, render_template};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    env,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

mod herdr;
mod models;
mod tmux;
mod zellij;
pub use models::{LaunchResponse, LaunchTarget};

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
    validate_names(&workspace, &tab)?;
    Ok((workspace, tab))
}

pub fn validate_names(workspace: &str, tab: &str) -> Result<()> {
    for label in [workspace, tab] {
        ensure!(
            !label.trim().is_empty() && label.len() <= 256 && !label.chars().any(char::is_control),
            "rendered workspace and tab names must be nonempty, single-line, and at most 256 UTF-8 bytes"
        );
    }
    Ok(())
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
            herdr::Client::new(cfg).execute(&herdr::ListWorkspaces)?;
        }
        Multiplexer::Tmux => {
            agent_executable(cfg)?;
            tmux::HasSession {
                session: &cfg.tmux_session,
            }
            .execute(cfg)?;
        }
        Multiplexer::Zellij => {
            agent_executable(cfg)?;
            zellij::QueryTabs {
                session: &cfg.zellij_session,
            }
            .execute(cfg)?;
        }
    }
    Ok(())
}

pub fn launch(
    cfg: &Config,
    request: &LaunchRequest<'_>,
    mut checkpoint: impl FnMut(&LaunchTarget) -> Result<()>,
) -> Result<LaunchResponse> {
    let prompt = format!(
        "{}\n\nWhen posting a new comment on GitHub, start its body with \"{}:\".",
        request.prompt, request.agent_name
    );
    let target = match cfg.multiplexer {
        Multiplexer::Herdr => {
            let client = herdr::Client::new(cfg);
            let workspace = client.execute(&herdr::CreateWorkspace {
                directory: request.directory,
                label: request.workspace_name,
            })?;
            let pane = workspace.pane()?.clone();
            let target = LaunchTarget::HerdrPane(pane.clone());
            checkpoint(&target)?;
            client.execute(&herdr::RenameTab {
                tab: workspace.tab()?,
                label: request.tab_name,
            })?;
            client.execute(&herdr::StartAgent {
                name: request.agent_name,
                kind: &cfg.agent,
                pane: &pane,
                arguments: &cfg.agent_args,
            })?;
            client.execute(&herdr::PromptAgent {
                name: request.agent_name,
                text: &prompt,
            })?;
            target
        }
        Multiplexer::Tmux => {
            let executable = agent_executable(cfg)?;
            let response = tmux::NewWindow {
                session: &cfg.tmux_session,
                directory: request.directory,
                name: request.tab_name,
                executable: &executable,
                arguments: &cfg.agent_args,
                prompt: &prompt,
            }
            .execute(cfg)?;
            let target = LaunchTarget::TmuxPane(response.pane);
            checkpoint(&target)?;
            target
        }
        Multiplexer::Zellij => {
            let executable = agent_executable(cfg)?;
            let response = zellij::NewTab {
                session: &cfg.zellij_session,
                directory: request.directory,
                name: request.tab_name,
                executable: &executable,
                arguments: &cfg.agent_args,
                prompt: &prompt,
            }
            .execute(cfg)?;
            let target = LaunchTarget::ZellijTab(response.tab);
            checkpoint(&target)?;
            target
        }
    };
    Ok(LaunchResponse { target })
}
