//! Machine-local, durable schedules for agent prompts and literal command argv.
use super::Trigger;
use crate::{Config, expand_home, multiplexer, render_template, run_in_directory};
use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Duration as TimeDelta, SecondsFormat, Utc};
use chrono_tz::Tz;
use croner::Cron;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

fn yes() -> bool {
    true
}
fn utc() -> String {
    "UTC".into()
}
fn workspace() -> String {
    "{task_name} {scheduled_at}".into()
}
fn tab() -> String {
    "{agent_name}".into()
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Task {
    pub name: String,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub cron: Option<String>,
    #[serde(default)]
    pub every_seconds: Option<u64>,
    #[serde(default = "utc")]
    pub timezone: String,
    #[serde(default)]
    pub directory: Option<PathBuf>,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub command: Vec<String>,
    #[serde(default)]
    pub run_immediately: bool,
    #[serde(default)]
    pub timeout_seconds: Option<u64>,
    #[serde(default = "workspace")]
    pub workspace_name: String,
    #[serde(default = "tab")]
    pub tab_name: String,
}

pub fn validate(tasks: &mut [Task], default_directory: &Path, default_timeout: u64) -> Result<()> {
    let mut names = BTreeSet::new();
    for task in tasks {
        ensure!(
            !task.name.is_empty()
                && task.name.len() <= 64
                && task
                    .name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b)),
            "scheduled task names must use 1–64 letters, digits, underscores, or hyphens"
        );
        ensure!(
            names.insert(task.name.clone()),
            "duplicate scheduled task name {}",
            task.name
        );
        if !task.enabled {
            continue;
        }
        let timing = Timing::new(task).with_context(|| format!("schedule {}", task.name))?;
        timing
            .after(Utc::now())
            .with_context(|| format!("schedule {} has no next occurrence", task.name))?;
        ensure!(
            task.prompt
                .as_ref()
                .is_some_and(|prompt| !prompt.trim().is_empty())
                ^ !task.command.is_empty(),
            "schedule {} needs exactly one nonempty prompt or command argv",
            task.name
        );
        ensure!(
            task.prompt.is_none() || task.command.is_empty(),
            "schedule {} cannot have both prompt and command",
            task.name
        );
        ensure!(
            task.command
                .first()
                .is_none_or(|program| !program.is_empty()),
            "schedule {} has an empty command executable",
            task.name
        );
        ensure!(
            task.timeout_seconds.unwrap_or(default_timeout) > 0,
            "schedule {} timeout must be positive",
            task.name
        );
        for label in [&task.workspace_name, &task.tab_name] {
            ensure!(
                !label.trim().is_empty() && !label.chars().any(char::is_control),
                "schedule {} needs nonempty single-line name templates",
                task.name
            );
        }
        task.directory = Some(expand_home(
            task.directory.as_deref().unwrap_or(default_directory),
        )?);
    }
    Ok(())
}

enum Timing {
    Interval(TimeDelta),
    Cron(Box<Cron>, Tz),
}
impl Timing {
    fn new(task: &Task) -> Result<Self> {
        let timezone: Tz = task.timezone.parse().context("invalid schedule timezone")?;
        match (&task.cron, task.every_seconds) {
            (Some(expression), None) => Ok(Self::Cron(
                Box::new(expression.parse().context("invalid cron expression")?),
                timezone,
            )),
            (None, Some(seconds)) => {
                ensure!(seconds > 0, "schedule interval must be positive");
                let duration = i64::try_from(seconds)
                    .ok()
                    .and_then(TimeDelta::try_seconds)
                    .context("schedule interval is too large")?;
                Ok(Self::Interval(duration))
            }
            _ => anyhow::bail!("configure exactly one of cron or every_seconds"),
        }
    }
    fn after(&self, now: DateTime<Utc>) -> Result<DateTime<Utc>> {
        match self {
            Self::Interval(duration) => now
                .checked_add_signed(*duration)
                .context("schedule date overflow"),
            Self::Cron(cron, timezone) => Ok(cron
                .find_next_occurrence(&now.with_timezone(timezone), false)?
                .with_timezone(&Utc)),
        }
    }
}

#[derive(Debug)]
pub struct ApiError {
    pub status: u16,
    message: String,
}
impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for ApiError {}
pub fn problem(status: u16, message: impl Into<String>) -> anyhow::Error {
    ApiError {
        status,
        message: message.into(),
    }
    .into()
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Run {
    pub id: u64,
    pub scheduled_at: DateTime<Utc>,
    pub status: String,
    pub agent_name: Option<String>,
    pub pane: Option<String>,
    pub error: Option<String>,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Job {
    pub task: Task,
    pub revision: u64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub next_run: Option<DateTime<Utc>>,
    pub last_run: Option<Run>,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Database {
    version: u32,
    jobs: BTreeMap<String, Job>,
}

pub struct Store {
    database: Mutex<Database>,
    path: PathBuf,
    default_directory: PathBuf,
    default_timeout: u64,
}
impl Store {
    pub fn open(cfg: &Config, state_dir: &Path) -> Result<Self> {
        let path = state_dir.join("schedules.json");
        let mut database: Database = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .context("invalid schedule store; refusing to forget dispatched tasks")?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Database {
                version: 1,
                jobs: BTreeMap::new(),
            },
            Err(error) => return Err(error.into()),
        };
        ensure!(
            database.version == 1,
            "unsupported schedule store version {}",
            database.version
        );
        for job in database.jobs.values_mut() {
            validate(
                std::slice::from_mut(&mut job.task),
                &cfg.repos_directory,
                cfg.command_timeout_seconds,
            )?;
            if let Some(run) = &mut job.last_run
                && run.status == "reserved"
            {
                run.status = "needs-attention".into();
                run.error = Some("Service stopped during dispatch; review the previous work and use schedule resume.".into());
                job.revision += 1;
            }
        }
        write_json(&path, &database)?;
        Ok(Self {
            database: Mutex::new(database),
            path,
            default_directory: cfg.repos_directory.clone(),
            default_timeout: cfg.command_timeout_seconds,
        })
    }
    fn transaction<T>(&self, edit: impl FnOnce(&mut Database) -> Result<T>) -> Result<T> {
        let mut current = self
            .database
            .lock()
            .map_err(|_| anyhow::anyhow!("schedule store lock poisoned"))?;
        let mut next = current.clone();
        let output = edit(&mut next)?;
        write_json(&self.path, &next)?;
        *current = next;
        Ok(output)
    }
    pub fn list(&self) -> Result<Vec<Job>> {
        Ok(self
            .database
            .lock()
            .map_err(|_| anyhow::anyhow!("schedule store lock poisoned"))?
            .jobs
            .values()
            .cloned()
            .collect())
    }
    pub fn get(&self, name: &str) -> Result<Job> {
        self.database
            .lock()
            .map_err(|_| anyhow::anyhow!("schedule store lock poisoned"))?
            .jobs
            .get(name)
            .cloned()
            .ok_or_else(|| problem(404, format!("schedule {name} not found")))
    }
    fn validated(&self, value: serde_json::Value) -> Result<Task> {
        let mut task: Task =
            serde_json::from_value(value).map_err(|error| problem(400, error.to_string()))?;
        validate(
            std::slice::from_mut(&mut task),
            &self.default_directory,
            self.default_timeout,
        )
        .map_err(|error| problem(400, format!("{error:#}")))?;
        Ok(task)
    }
    fn next(task: &Task, now: DateTime<Utc>) -> Result<Option<DateTime<Utc>>> {
        if !task.enabled {
            return Ok(None);
        }
        if task.run_immediately {
            return Ok(Some(now));
        }
        Ok(Some(Timing::new(task)?.after(now)?))
    }
    pub fn create(&self, value: serde_json::Value) -> Result<Job> {
        let mut task = self.validated(value)?;
        let now = Utc::now();
        let next_run = Self::next(&task, now)?;
        task.run_immediately = false;
        self.transaction(|database| {
            if database.jobs.contains_key(&task.name) {
                return Err(problem(
                    409,
                    format!("schedule {} already exists", task.name),
                ));
            }
            let job = Job {
                task,
                revision: 1,
                created_at: now,
                updated_at: now,
                next_run,
                last_run: None,
            };
            database.jobs.insert(job.task.name.clone(), job.clone());
            Ok(job)
        })
    }
    fn editable(job: &Job, revision: Option<u64>) -> Result<()> {
        if revision.is_some_and(|revision| revision != job.revision) {
            return Err(problem(
                412,
                "schedule revision changed; read it again before editing",
            ));
        }
        if job
            .last_run
            .as_ref()
            .is_some_and(|run| run.status == "reserved")
        {
            return Err(problem(
                409,
                "schedule is dispatching; retry after the current dispatch finishes",
            ));
        }
        Ok(())
    }
    pub fn update(
        &self,
        name: &str,
        value: serde_json::Value,
        replace: bool,
        revision: Option<u64>,
    ) -> Result<Job> {
        self.transaction(|database| {
            let job = database
                .jobs
                .get_mut(name)
                .ok_or_else(|| problem(404, format!("schedule {name} not found")))?;
            Self::editable(job, revision)?;
            let mut fields = if replace {
                serde_json::Map::new()
            } else {
                serde_json::to_value(&job.task)?
                    .as_object()
                    .unwrap()
                    .clone()
            };
            let updates = value
                .as_object()
                .ok_or_else(|| problem(400, "schedule body must be an object"))?;
            fields.extend(updates.clone());
            if fields
                .get("name")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|updated| updated != name)
            {
                return Err(problem(
                    400,
                    "schedule names are immutable; create a new schedule to rename it",
                ));
            }
            fields.insert("name".into(), serde_json::Value::String(name.into()));
            let mut task = self.validated(serde_json::Value::Object(fields))?;
            job.next_run = Self::next(&task, Utc::now())?;
            task.run_immediately = false;
            job.task = task;
            job.revision += 1;
            job.updated_at = Utc::now();
            Ok(job.clone())
        })
    }
    pub fn delete(&self, name: &str, revision: Option<u64>) -> Result<()> {
        self.transaction(|database| {
            let job = database
                .jobs
                .get(name)
                .ok_or_else(|| problem(404, format!("schedule {name} not found")))?;
            Self::editable(job, revision)?;
            database.jobs.remove(name);
            Ok(())
        })
    }
    pub fn resume(&self, name: &str, revision: Option<u64>) -> Result<Job> {
        self.transaction(|database| {
            let job = database
                .jobs
                .get_mut(name)
                .ok_or_else(|| problem(404, format!("schedule {name} not found")))?;
            Self::editable(job, revision)?;
            let run = job
                .last_run
                .as_mut()
                .filter(|run| run.status == "needs-attention")
                .ok_or_else(|| problem(409, "schedule does not need recovery"))?;
            run.status = "acknowledged".into();
            job.next_run = Self::next(&job.task, Utc::now())?;
            job.revision += 1;
            job.updated_at = Utc::now();
            Ok(job.clone())
        })
    }
    fn claim(&self, name: &str, revision: u64) -> Result<Option<Job>> {
        self.transaction(|database| {
            let Some(job) = database.jobs.get_mut(name) else {
                return Ok(None);
            };
            if job.revision != revision
                || !job.task.enabled
                || job.next_run.is_none_or(|next| next > Utc::now())
                || job.last_run.as_ref().is_some_and(|run| {
                    matches!(run.status.as_str(), "reserved" | "needs-attention")
                })
            {
                return Ok(None);
            }
            let scheduled_at = job.next_run.unwrap();
            job.revision += 1;
            let agent_name = job
                .task
                .prompt
                .as_ref()
                .map(|_| format!("schedule-{}-{}", name, job.revision));
            job.last_run = Some(Run {
                id: job.revision,
                scheduled_at,
                status: "reserved".into(),
                agent_name,
                pane: None,
                error: None,
            });
            job.next_run = Some(Timing::new(&job.task)?.after(Utc::now())?);
            job.updated_at = Utc::now();
            Ok(Some(job.clone()))
        })
    }
    fn checkpoint(&self, name: &str, id: u64, pane: &str) -> Result<()> {
        self.transaction(|database| {
            let job = database
                .jobs
                .get_mut(name)
                .context("dispatched schedule disappeared")?;
            let run = job
                .last_run
                .as_mut()
                .filter(|run| run.id == id)
                .context("schedule dispatch changed")?;
            run.pane = Some(pane.into());
            job.revision += 1;
            Ok(())
        })
    }
    fn finish(&self, name: &str, id: u64, result: &Result<()>) -> Result<()> {
        self.transaction(|database| {
            let job = database
                .jobs
                .get_mut(name)
                .context("dispatched schedule disappeared")?;
            let run = job
                .last_run
                .as_mut()
                .filter(|run| run.id == id)
                .context("schedule dispatch changed")?;
            run.status = match (result.is_ok(), job.task.prompt.is_some()) {
                (true, true) => "started",
                (true, false) => "succeeded",
                (false, true) => "needs-attention",
                (false, false) => "failed",
            }
            .into();
            run.error = result.as_ref().err().map(|error| format!("{error:#}"));
            job.next_run = if job.task.enabled {
                Some(Timing::new(&job.task)?.after(Utc::now())?)
            } else {
                None
            };
            job.revision += 1;
            job.updated_at = Utc::now();
            Ok(())
        })
    }
}

pub fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let temp = path.with_extension("json.tmp");
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(&temp)?;
    file.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    file.write_all(&serde_json::to_vec_pretty(value)?)?;
    file.sync_all()?;
    fs::rename(&temp, path)?;
    File::open(path.parent().unwrap())?.sync_all()?;
    Ok(())
}

pub struct ScheduleTrigger<'a> {
    cfg: &'a Config,
    store: Arc<Store>,
}
impl<'a> ScheduleTrigger<'a> {
    pub fn new(cfg: &'a Config, store: Arc<Store>) -> Self {
        Self { cfg, store }
    }
    fn dispatch(&self, snapshot: &Job) -> Result<()> {
        let task = &snapshot.task;
        let directory = task
            .directory
            .as_deref()
            .context("scheduled task directory was not resolved")?;
        ensure!(
            directory.is_dir(),
            "task directory {} does not exist",
            directory.display()
        );
        if task.prompt.is_some() {
            multiplexer::ensure_ready(self.cfg)?;
        }
        let Some(job) = self.store.claim(&task.name, snapshot.revision)? else {
            return Ok(());
        };
        let run = job.last_run.as_ref().unwrap();
        let scheduled = run.scheduled_at.to_rfc3339_opts(SecondsFormat::Secs, true);
        let agent_name = run.agent_name.as_deref().unwrap_or("");
        let values = [
            ("{task_name}", task.name.as_str()),
            ("{scheduled_at}", scheduled.as_str()),
            ("{agent_name}", agent_name),
            ("{agent}", self.cfg.agent.as_str()),
            ("{multiplexer}", self.cfg.multiplexer.as_str()),
            ("{session}", multiplexer::session(self.cfg)),
        ];
        let result = if let Some(prompt) = &task.prompt {
            let workspace_name = render_template(&task.workspace_name, &values);
            let tab_name = render_template(&task.tab_name, &values);
            (|| -> Result<()> {
                multiplexer::validate_names(&workspace_name, &tab_name)?;
                let prompt = format!(
                    "Scheduled task {}.\nScheduled occurrence: {scheduled}.\n\n{prompt}\n\nFollow the directory's AGENTS.md instructions. Manage persistent schedules with invocateur schedule --help.",
                    task.name
                );
                multiplexer::launch(
                    self.cfg,
                    &multiplexer::LaunchRequest {
                        directory,
                        agent_name,
                        prompt: &prompt,
                        workspace_name: &workspace_name,
                        tab_name: &tab_name,
                    },
                    |pane| self.store.checkpoint(&task.name, run.id, pane),
                )
            })()
        } else {
            run_in_directory(
                &task.command[0],
                &task.command[1..],
                task.timeout_seconds
                    .unwrap_or(self.cfg.command_timeout_seconds),
                Some(directory),
            )
            .map(|output| {
                if !output.trim().is_empty() {
                    eprintln!("schedule {} output:\n{}", task.name, output.trim_end());
                }
            })
        };
        self.store.finish(&task.name, run.id, &result)?;
        if result.is_ok() {
            eprintln!("schedule {}: dispatched occurrence {scheduled}", task.name);
        }
        result
    }
}
impl Trigger for ScheduleTrigger<'_> {
    fn name(&self) -> &str {
        "schedule"
    }
    fn poll(&mut self) -> Result<()> {
        let mut failed = false;
        for job in self
            .store
            .list()?
            .into_iter()
            .filter(|job| job.task.enabled)
        {
            if job
                .last_run
                .as_ref()
                .is_some_and(|run| run.status == "needs-attention")
            {
                eprintln!(
                    "schedule {} is paused: review previous work and use schedule resume",
                    job.task.name
                );
                failed = true;
                continue;
            }
            if job.next_run.is_none_or(|next| next > Utc::now()) {
                continue;
            }
            if let Err(error) = self.dispatch(&job) {
                eprintln!("schedule {}: {error:#}", job.task.name);
                failed = true;
            }
        }
        ensure!(!failed, "one or more scheduled tasks could not run");
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cron_uses_iana_wall_clock_time_across_daylight_saving_changes() {
        let task: Task = serde_json::from_value(serde_json::json!({
            "name":"review", "cron":"0 9 * * MON-FRI", "timezone":"America/New_York", "command":["true"]
        })).unwrap();
        let timing = Timing::new(&task).unwrap();
        for (now, expected) in [
            ("2026-10-30T12:59:00Z", "2026-10-30T13:00:00Z"),
            ("2026-11-02T13:59:00Z", "2026-11-02T14:00:00Z"),
            ("2026-10-31T00:00:00Z", "2026-11-02T14:00:00Z"),
        ] {
            let now: DateTime<Utc> = now.parse().unwrap();
            let expected: DateTime<Utc> = expected.parse().unwrap();
            assert_eq!(timing.after(now).unwrap(), expected);
        }
    }
}
