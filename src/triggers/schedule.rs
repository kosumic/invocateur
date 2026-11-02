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
    fs::{self, File},
    io::Write,
    path::{Path, PathBuf},
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
    pub run_on_startup: bool,
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

#[derive(Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct Definition {
    cron: Option<String>,
    every_seconds: Option<u64>,
    timezone: String,
}
impl From<&Task> for Definition {
    fn from(task: &Task) -> Self {
        Self {
            cron: task.cron.clone(),
            every_seconds: task.every_seconds,
            timezone: task.timezone.clone(),
        }
    }
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

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Run {
    scheduled_at: DateTime<Utc>,
    status: String,
    agent_name: Option<String>,
    pane: Option<String>,
    error: Option<String>,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Job {
    definition: Definition,
    next_run: DateTime<Utc>,
    last_run: Option<Run>,
}
#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct State {
    jobs: BTreeMap<String, Job>,
}

pub struct ScheduleTrigger<'a> {
    cfg: &'a Config,
    state_file: PathBuf,
    state: State,
    timings: BTreeMap<String, Timing>,
}
impl<'a> ScheduleTrigger<'a> {
    pub fn new(cfg: &'a Config, state_dir: &Path) -> Result<Self> {
        let state_file = state_dir.join("schedules.json");
        let mut state: State = match fs::read(&state_file) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .context("invalid schedules state; refusing to forget dispatched tasks")?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => State::default(),
            Err(error) => return Err(error.into()),
        };
        let mut timings = BTreeMap::new();
        let now = Utc::now();
        for task in cfg.scheduled_tasks.iter().filter(|task| task.enabled) {
            let timing = Timing::new(task)?;
            let definition = Definition::from(task);
            let existing = state.jobs.get_mut(&task.name);
            if let Some(existing) = existing {
                if existing.definition != definition {
                    existing.definition = definition;
                    existing.next_run = timing.after(now)?;
                }
            } else {
                let next_run = if task.run_on_startup {
                    now
                } else {
                    timing.after(now)?
                };
                state.jobs.insert(
                    task.name.clone(),
                    Job {
                        definition,
                        next_run,
                        last_run: None,
                    },
                );
            }
            timings.insert(task.name.clone(), timing);
        }
        let trigger = Self {
            cfg,
            state_file,
            state,
            timings,
        };
        trigger.save()?;
        Ok(trigger)
    }
    fn save(&self) -> Result<()> {
        let temp = self.state_file.with_extension("json.tmp");
        let mut file = File::create(&temp)?;
        file.write_all(&serde_json::to_vec_pretty(&self.state)?)?;
        file.sync_all()?;
        fs::rename(&temp, &self.state_file)?;
        File::open(self.state_file.parent().unwrap())?.sync_all()?;
        Ok(())
    }
    fn dispatch(&mut self, task: &Task) -> Result<()> {
        let scheduled_at = self.state.jobs[&task.name].next_run;
        let directory = task
            .directory
            .as_deref()
            .context("scheduled task directory was not resolved")?;
        ensure!(
            directory.is_dir(),
            "task directory {} does not exist",
            directory.display()
        );
        let agent_name = format!("schedule-{}-{}", task.name, scheduled_at.timestamp());
        let scheduled = scheduled_at.to_rfc3339_opts(SecondsFormat::Secs, true);
        let values = [
            ("{task_name}", task.name.as_str()),
            ("{scheduled_at}", scheduled.as_str()),
            ("{agent_name}", agent_name.as_str()),
            ("{agent}", self.cfg.agent.as_str()),
            ("{multiplexer}", self.cfg.multiplexer.as_str()),
            ("{session}", multiplexer::session(self.cfg)),
        ];
        let workspace_name = render_template(&task.workspace_name, &values);
        let tab_name = render_template(&task.tab_name, &values);
        if task.prompt.is_some() {
            multiplexer::validate_names(&workspace_name, &tab_name)?;
            multiplexer::ensure_ready(self.cfg)?;
        }
        let next_run = self.timings[&task.name].after(Utc::now())?;
        let job = self.state.jobs.get_mut(&task.name).unwrap();
        job.next_run = next_run;
        job.last_run = Some(Run {
            scheduled_at,
            status: "reserved".into(),
            agent_name: task.prompt.as_ref().map(|_| agent_name.clone()),
            pane: None,
            error: None,
        });
        self.save()?; // Reserve durably before any task side effect.
        let result = if let Some(prompt) = &task.prompt {
            let prompt = format!(
                "Scheduled task {}.\nScheduled occurrence: {scheduled}.\n\n{prompt}\n\nFollow the directory's AGENTS.md instructions.",
                task.name
            );
            let request = multiplexer::LaunchRequest {
                directory,
                agent_name: &agent_name,
                prompt: &prompt,
                workspace_name: &workspace_name,
                tab_name: &tab_name,
            };
            multiplexer::launch(self.cfg, &request, |pane| {
                self.state
                    .jobs
                    .get_mut(&task.name)
                    .unwrap()
                    .last_run
                    .as_mut()
                    .unwrap()
                    .pane = Some(pane.into());
                self.save()
            })
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
        let job = self.state.jobs.get_mut(&task.name).unwrap();
        let record = job.last_run.as_mut().unwrap();
        match &result {
            Ok(()) => {
                record.status = if task.prompt.is_some() {
                    "started"
                } else {
                    "succeeded"
                }
                .into();
                eprintln!("schedule {}: {} at {scheduled}", task.name, record.status);
            }
            Err(error) => {
                record.status = if task.prompt.is_some() {
                    "needs-attention"
                } else {
                    "failed"
                }
                .into();
                record.error = Some(format!("{error:#}"));
            }
        }
        job.next_run = self.timings[&task.name].after(Utc::now())?;
        self.save()?;
        result
    }
}
impl Trigger for ScheduleTrigger<'_> {
    fn name(&self) -> &str {
        "schedule"
    }
    fn poll(&mut self) -> Result<()> {
        let cfg = self.cfg;
        let mut failed = false;
        for task in cfg.scheduled_tasks.iter().filter(|task| task.enabled) {
            let job = &self.state.jobs[&task.name];
            if job
                .last_run
                .as_ref()
                .is_some_and(|run| matches!(run.status.as_str(), "reserved" | "needs-attention"))
            {
                eprintln!(
                    "schedule {} is paused: previous dispatch needs attention; review schedules.json before resuming",
                    task.name
                );
                failed = true;
                continue;
            }
            if job.next_run > Utc::now() {
                continue;
            }
            if let Err(error) = self.dispatch(task) {
                eprintln!("schedule {}: {error:#}", task.name);
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
