//! Running user scripts around clients and jobs.
//!
//! Scripts receive their context through `ELKARBACKUP_*` environment
//! variables, the same names the original application used, so existing
//! scripts keep working.

use std::collections::BTreeMap;

use elkar_core::script::{self, Level, Phase, Script};
use tokio_util::sync::CancellationToken;

use crate::EngineConfig;
use crate::activity::SubjectLog;
use crate::process::{CommandSpec, Outcome, ProcessRunner};

/// Script output is truncated to this many characters in activity records.
const MAX_LOGGED_OUTPUT: usize = 500;

/// Value of `ELKARBACKUP_STATUS` given to post scripts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunStatus {
    Ok,
    BackupFailed,
    PreScriptsFailed,
    Aborted,
}

impl RunStatus {
    pub fn code(self) -> u8 {
        match self {
            Self::Ok => 0,
            Self::BackupFailed => 1,
            Self::PreScriptsFailed => 2,
            Self::Aborted => 3,
        }
    }
}

/// Environment passed to scripts, built by the caller per level.
#[derive(Debug, Clone, Default)]
pub struct ScriptEnv(BTreeMap<String, String>);

impl ScriptEnv {
    pub fn new(level: Level, phase: Phase, status: RunStatus) -> Self {
        Self::default()
            .set("LEVEL", level)
            .set("EVENT", phase)
            .set("STATUS", status.code())
    }

    /// Sets `ELKARBACKUP_<name>`.
    pub fn set(mut self, name: &str, value: impl ToString) -> Self {
        self.0.insert(format!("ELKARBACKUP_{name}"), value.to_string());
        self
    }
}

pub struct ScriptRunner<'a> {
    pub config: &'a EngineConfig,
    pub runner: &'a dyn ProcessRunner,
}

impl ScriptRunner<'_> {
    /// Runs `scripts` in phase order, stopping at the first failure.
    /// Returns whether every script succeeded.
    pub async fn run_all(
        &self,
        mut scripts: Vec<Script>,
        phase: Phase,
        env: &ScriptEnv,
        log: &mut SubjectLog,
        cancel: &CancellationToken,
    ) -> bool {
        if scripts.is_empty() {
            return true;
        }
        script::sort_for_phase(&mut scripts, phase);
        for script in scripts {
            if !self.run_one(&script, phase, env, log, cancel).await {
                return false;
            }
        }
        true
    }

    async fn run_one(
        &self,
        script: &Script,
        phase: Phase,
        env: &ScriptEnv,
        log: &mut SubjectLog,
        cancel: &CancellationToken,
    ) -> bool {
        let name = &script.spec.name;
        let path = self.config.scripts_dir.join(script.file_name());
        if !path.is_file() {
            log.error(format!(
                "{phase} script \"{name}\" is configured but file {} is missing",
                path.display()
            ))
            .await;
            return false;
        }
        let mut spec = CommandSpec::new(path.to_string_lossy()).prefixed(&self.config.script_prefix);
        spec.env.extend(env.0.clone());

        match self.runner.run(&spec, cancel).await {
            Ok(Outcome::Finished {
                success: true, output, ..
            }) => {
                log.info(format!("{phase} script \"{name}\" succeeded.{}", excerpt(&output)))
                    .await;
                true
            }
            Ok(Outcome::Finished { output, code, .. }) => {
                let code = code.map_or_else(|| "signal".to_owned(), |c| c.to_string());
                log.error(format!(
                    "{phase} script \"{name}\" failed (exit {code}).{}",
                    excerpt(&output)
                ))
                .await;
                false
            }
            Ok(Outcome::Cancelled) => {
                log.warn(format!("{phase} script \"{name}\" cancelled")).await;
                false
            }
            Err(err) => {
                log.error(format!("{phase} script \"{name}\" could not be started: {err}"))
                    .await;
                false
            }
        }
    }
}

/// Formats command output for an activity record: a leading newline and at
/// most [`MAX_LOGGED_OUTPUT`] characters.
pub(crate) fn excerpt(output: &str) -> String {
    let trimmed = output.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let mut text: String = trimmed.chars().take(MAX_LOGGED_OUTPUT).collect();
    if text.len() < trimmed.len() {
        text.push_str(" (...)");
    }
    format!(" Output:\n{text}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn excerpt_truncates() {
        assert_eq!(excerpt("  "), "");
        let long = "x".repeat(600);
        let e = excerpt(&long);
        assert!(e.ends_with(" (...)"));
        assert_eq!(e.chars().filter(|c| *c == 'x').count(), MAX_LOGGED_OUTPUT);
    }

    #[test]
    fn env_uses_legacy_names() {
        let env = ScriptEnv::new(Level::Job, Phase::Post, RunStatus::Aborted).set("JOB_NAME", "etc");
        assert_eq!(env.0["ELKARBACKUP_LEVEL"], "JOB");
        assert_eq!(env.0["ELKARBACKUP_EVENT"], "POST");
        assert_eq!(env.0["ELKARBACKUP_STATUS"], "3");
        assert_eq!(env.0["ELKARBACKUP_JOB_NAME"], "etc");
    }
}
