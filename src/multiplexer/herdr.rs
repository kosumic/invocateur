//! Typed CLI requests and forward-compatible Herdr response models.
use super::models::{HerdrPaneId, HerdrTabId, HerdrWorkspaceId};
use crate::{Config, args, run_command};
use anyhow::{Context, Result, bail, ensure};
use serde::{
    Deserialize,
    de::{DeserializeOwned, IgnoredAny},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    path::Path,
};

pub trait Request {
    type Response: DeserializeOwned;
    const OPERATION: &'static str;
    fn argv(&self) -> Vec<String>;
}
// JSON object shapes are part of the wire contract; positional arrays are not.
#[derive(Deserialize)]
struct Object<T> {
    #[serde(flatten)]
    value: T,
}
#[derive(Deserialize)]
struct Envelope<T> {
    result: Option<Object<T>>,
    error: Option<Object<RemoteError>>,
}
#[derive(Debug, Deserialize)]
pub struct RemoteError {
    pub code: String,
    pub message: String,
}
impl fmt::Display for RemoteError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "Herdr error {}: {}", self.code, self.message)
    }
}
impl std::error::Error for RemoteError {}

pub struct Client<'a> {
    cfg: &'a Config,
}
impl<'a> Client<'a> {
    pub fn new(cfg: &'a Config) -> Self {
        Self { cfg }
    }
    pub fn execute<R: Request>(&self, request: &R) -> Result<R::Response> {
        let mut argv = args(&["--session", &self.cfg.herdr_session]);
        argv.extend(request.argv());
        let output = run_command(
            &self.cfg.herdr_path,
            &argv,
            self.cfg.command_timeout_seconds,
            None,
        )?;
        let bytes = if output.stdout.is_empty() && !output.status.success() {
            &output.stderr
        } else {
            &output.stdout
        };
        let envelope: Envelope<R::Response> =
            match serde_json::from_slice::<Object<Envelope<R::Response>>>(bytes)
                .map(|object| object.value)
            {
                Ok(envelope) => envelope,
                Err(error) => {
                    output.ensure_success(&self.cfg.herdr_path)?;
                    return Err(error)
                        .with_context(|| format!("invalid Herdr {} response", R::OPERATION));
                }
            };
        match (envelope.result, envelope.error) {
            (Some(result), None) => {
                output.ensure_success(&self.cfg.herdr_path)?;
                Ok(result.value)
            }
            (None, Some(error)) => {
                Err(error.value).with_context(|| format!("Herdr {} failed", R::OPERATION))
            }
            (Some(_), Some(_)) => bail!(
                "invalid Herdr {} response: both result and error",
                R::OPERATION
            ),
            (None, None) => bail!(
                "invalid Herdr {} response: missing result and error",
                R::OPERATION
            ),
        }
    }
}

#[derive(Deserialize)]
pub struct Acknowledged {
    #[serde(flatten)]
    _extensions: BTreeMap<String, IgnoredAny>,
}
#[derive(Deserialize)]
pub struct WorkspaceSummary {
    #[serde(rename = "workspace_id")]
    _id: HerdrWorkspaceId,
}
#[derive(Deserialize)]
pub struct Workspaces {
    #[serde(rename = "workspaces")]
    _workspaces: Vec<Object<WorkspaceSummary>>,
}
pub struct ListWorkspaces;
impl Request for ListWorkspaces {
    type Response = Workspaces;
    const OPERATION: &'static str = "workspace.list";
    fn argv(&self) -> Vec<String> {
        args(&["workspace", "list"])
    }
}

// Preserve a usable pane checkpoint even if another field in a create reply is bad.
#[derive(Deserialize)]
#[serde(untagged)]
enum Field<T> {
    Valid(T),
    Invalid(IgnoredAny),
}
impl<T> Default for Field<T> {
    fn default() -> Self {
        Self::Invalid(IgnoredAny)
    }
}
impl<T> Field<T> {
    fn get(&self, description: &str) -> Result<&T> {
        match self {
            Self::Valid(value) => Ok(value),
            Self::Invalid(_) => bail!("Herdr did not return a valid {description}"),
        }
    }
}
#[derive(Deserialize)]
struct RootPane {
    #[serde(default)]
    pane_id: Field<HerdrPaneId>,
}
#[derive(Deserialize)]
struct Tab {
    #[serde(default)]
    tab_id: Field<HerdrTabId>,
}
#[derive(Deserialize)]
pub struct CreatedWorkspace {
    #[serde(default)]
    root_pane: Field<Object<RootPane>>,
    #[serde(default)]
    tab: Field<Object<Tab>>,
}
impl CreatedWorkspace {
    pub fn pane(&self) -> Result<&HerdrPaneId> {
        self.root_pane
            .get("root pane")?
            .value
            .pane_id
            .get("root pane ID")
    }
    pub fn tab(&self) -> Result<&HerdrTabId> {
        let tab = self.tab.get("tab")?.value.tab_id.get("root tab ID")?;
        ensure!(
            self.pane()?.as_str().split_once(':').unwrap().0
                == tab.as_str().split_once(':').unwrap().0,
            "Herdr returned pane and tab IDs from different workspaces"
        );
        Ok(tab)
    }
}
pub struct CreateWorkspace<'a> {
    pub directory: &'a Path,
    pub label: &'a str,
}
impl Request for CreateWorkspace<'_> {
    type Response = CreatedWorkspace;
    const OPERATION: &'static str = "workspace.create";
    fn argv(&self) -> Vec<String> {
        args(&[
            "workspace",
            "create",
            "--cwd",
            &self.directory.to_string_lossy(),
            "--label",
            self.label,
            "--no-focus",
        ])
    }
}
pub struct RenameTab<'a> {
    pub tab: &'a HerdrTabId,
    pub label: &'a str,
}
impl Request for RenameTab<'_> {
    type Response = Acknowledged;
    const OPERATION: &'static str = "tab.rename";
    fn argv(&self) -> Vec<String> {
        args(&["tab", "rename", self.tab.as_str(), self.label])
    }
}
pub struct StartAgent<'a> {
    pub name: &'a str,
    pub kind: &'a str,
    pub pane: &'a HerdrPaneId,
    pub arguments: &'a [String],
}
impl Request for StartAgent<'_> {
    type Response = Acknowledged;
    const OPERATION: &'static str = "agent.start";
    fn argv(&self) -> Vec<String> {
        let mut argv = args(&[
            "agent",
            "start",
            self.name,
            "--kind",
            self.kind,
            "--pane",
            self.pane.as_str(),
        ]);
        if !self.arguments.is_empty() {
            argv.push("--".into());
            argv.extend_from_slice(self.arguments);
        }
        argv
    }
}
pub struct PromptAgent<'a> {
    pub name: &'a str,
    pub text: &'a str,
}
impl Request for PromptAgent<'_> {
    type Response = Acknowledged;
    const OPERATION: &'static str = "agent.prompt";
    fn argv(&self) -> Vec<String> {
        args(&["agent", "prompt", self.name, self.text])
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum AgentStatus {
    Idle,
    Working,
    Blocked,
    Done,
    Unknown,
}
#[derive(Deserialize)]
struct AgentInfo {
    name: Option<String>,
    agent: Option<String>,
    agent_status: AgentStatus,
    #[serde(default)]
    launch_pending: bool,
    pane_id: HerdrPaneId,
}
#[derive(Deserialize)]
pub struct Agents {
    agents: Vec<Object<AgentInfo>>,
}
impl Agents {
    pub fn find(&self, names: &BTreeSet<String>) -> Result<Option<super::RunningAgent>> {
        let mut found = None;
        for object in &self.agents {
            let agent = &object.value;
            let Some(name) = agent.name.as_ref().filter(|name| names.contains(*name)) else {
                continue;
            };
            ensure!(
                found.is_none(),
                "multiple live Herdr agents match this conversation; inspect them before retrying"
            );
            ensure!(
                agent.agent.is_some(),
                "Herdr target {name} no longer hosts an agent"
            );
            ensure!(
                !agent.launch_pending,
                "Herdr agent {name} is still starting; retry on a later poll"
            );
            ensure!(
                matches!(
                    agent.agent_status,
                    AgentStatus::Idle | AgentStatus::Working | AgentStatus::Done
                ),
                "Herdr agent {name} is blocked or its state is unknown; not sending input or starting another agent"
            );
            found = Some(super::RunningAgent {
                name: name.clone(),
                target: super::LaunchTarget::HerdrPane(agent.pane_id.clone()),
            });
        }
        Ok(found)
    }
}
pub struct ListAgents;
impl Request for ListAgents {
    type Response = Agents;
    const OPERATION: &'static str = "agent.list";
    fn argv(&self) -> Vec<String> {
        args(&["agent", "list"])
    }
}
