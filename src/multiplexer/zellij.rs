//! Typed Zellij CLI requests and the documented numeric tab-creation response.
use super::models::ZellijTabId;
use crate::{Config, args, run};
use anyhow::{Context, Result};
use std::path::Path;

pub struct QueryTabs<'a> {
    pub session: &'a str,
}
pub struct SessionReady;
impl QueryTabs<'_> {
    pub fn execute(&self, cfg: &Config) -> Result<SessionReady> {
        run(
            &cfg.zellij_path,
            &args(&["--session", self.session, "action", "query-tab-names"]),
            cfg.command_timeout_seconds,
        )
        .context("start the configured Zellij session before using Invocateur")?;
        Ok(SessionReady)
    }
}
pub struct NewTab<'a> {
    pub session: &'a str,
    pub directory: &'a Path,
    pub name: &'a str,
    pub executable: &'a Path,
    pub arguments: &'a [String],
    pub prompt: &'a str,
}
pub struct CreatedTab {
    pub tab: ZellijTabId,
}
impl NewTab<'_> {
    pub fn execute(&self, cfg: &Config) -> Result<CreatedTab> {
        let mut argv = args(&[
            "--session",
            self.session,
            "action",
            "new-tab",
            "--no-focus",
            "--name",
            self.name,
            "--cwd",
            &self.directory.to_string_lossy(),
            "--",
            &self.executable.to_string_lossy(),
        ]);
        argv.extend_from_slice(self.arguments);
        argv.push(self.prompt.into());
        let output = run(&cfg.zellij_path, &argv, cfg.command_timeout_seconds)?;
        let tab = ZellijTabId::try_from(output.trim())
            .context("Zellij new-tab did not return a tab ID")?;
        Ok(CreatedTab { tab })
    }
}
