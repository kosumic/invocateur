//! tmux's supported CLI contract, including explicit machine output formatting.
use super::models::TmuxPaneId;
use crate::{Config, args, run};
use anyhow::{Context, Result};
use std::path::Path;

pub struct HasSession<'a> {
    pub session: &'a str,
}
pub struct SessionReady;
impl HasSession<'_> {
    pub fn execute(&self, cfg: &Config) -> Result<SessionReady> {
        run(
            &cfg.tmux_path,
            &args(&["has-session", "-t", &format!("={}", self.session)]),
            cfg.command_timeout_seconds,
        )
        .context("start the configured tmux session before using Invocateur")?;
        Ok(SessionReady)
    }
}
pub struct NewWindow<'a> {
    pub session: &'a str,
    pub directory: &'a Path,
    pub name: &'a str,
    pub executable: &'a Path,
    pub arguments: &'a [String],
    pub prompt: &'a str,
}
pub struct CreatedWindow {
    pub pane: TmuxPaneId,
}
impl NewWindow<'_> {
    pub fn execute(&self, cfg: &Config) -> Result<CreatedWindow> {
        let quote = |value: &str| format!("'{}'", value.replace('\'', "'\\''"));
        let mut command = vec![quote(&self.executable.to_string_lossy())];
        command.extend(self.arguments.iter().map(|arg| quote(arg)));
        command.push(quote(self.prompt));
        let command = format!("exec {}", command.join(" "));
        let argv = args(&[
            "new-window",
            "-d",
            "-P",
            "-F",
            "#{pane_id}",
            "-t",
            &format!("={}:", self.session),
            "-n",
            self.name,
            "-c",
            &self.directory.to_string_lossy(),
            "--",
            "sh",
            "-c",
            &command,
        ]);
        // tmux treats a trailing semicolon as a command separator, including argv input.
        let argv: Vec<_> = argv
            .into_iter()
            .map(|arg| {
                arg.strip_suffix(';')
                    .map(|prefix| format!("{prefix}\\;"))
                    .unwrap_or(arg)
            })
            .collect();
        let output = run(&cfg.tmux_path, &argv, cfg.command_timeout_seconds)?;
        let pane = TmuxPaneId::try_from(output.trim().to_owned())
            .context("tmux new-window did not return a pane ID")?;
        Ok(CreatedWindow { pane })
    }
}
