use super::{Config, render_template};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, path::Path};

mod herdr;
mod models;
pub use models::{LaunchResponse, LaunchTarget};

#[derive(Clone, Copy, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Multiplexer {
    #[default]
    Herdr,
}

impl Multiplexer {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Herdr => "herdr",
        }
    }
}

pub fn session(cfg: &Config) -> &str {
    &cfg.herdr_session
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

pub fn ensure_ready(cfg: &Config) -> Result<()> {
    herdr::Client::new(cfg).execute(&herdr::ListWorkspaces)?;
    Ok(())
}

pub struct RunningAgent {
    pub name: String,
    pub target: LaunchTarget,
}

pub fn find_agent(cfg: &Config, names: &BTreeSet<String>) -> Result<Option<RunningAgent>> {
    if names.is_empty() {
        return Ok(None);
    }
    let response = herdr::Client::new(cfg).execute(&herdr::ListAgents)?;
    response.find(names)
}

pub fn input(cfg: &Config, name: &str, text: &str) -> Result<()> {
    let prompt = if cfg.prefix_github_comments {
        format!("{text}\n\nWhen posting a new comment on GitHub, start its body with \"{name}:\".")
    } else {
        text.to_owned()
    };
    herdr::Client::new(cfg).execute(&herdr::PromptAgent {
        name,
        text: &prompt,
    })?;
    Ok(())
}

pub fn launch(
    cfg: &Config,
    request: &LaunchRequest<'_>,
    mut checkpoint: impl FnMut(&LaunchTarget) -> Result<()>,
) -> Result<LaunchResponse> {
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
    input(cfg, request.agent_name, request.prompt)?;
    Ok(LaunchResponse { target })
}
