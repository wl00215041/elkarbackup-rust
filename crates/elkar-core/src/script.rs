//! User supplied scripts run before or after clients and jobs.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::ValidationErrors;
use crate::ids::ScriptId;

text_enum! {
    pub enum Phase {
        Pre => "PRE",
        Post => "POST",
    }
}

text_enum! {
    pub enum Level {
        Client => "CLIENT",
        Job => "JOB",
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScriptSpec {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub is_client_pre: bool,
    #[serde(default)]
    pub is_client_post: bool,
    #[serde(default)]
    pub is_job_pre: bool,
    #[serde(default)]
    pub is_job_post: bool,
}

impl ScriptSpec {
    pub fn validate(mut self) -> Result<Self, ValidationErrors> {
        self.name = self.name.trim().to_owned();
        let mut errors = ValidationErrors::new();
        errors.check(!self.name.is_empty(), "name", "name is required");
        errors.into_result().map(|()| self)
    }

    /// Whether the script may be attached at the given level and phase.
    pub fn allows(&self, level: Level, phase: Phase) -> bool {
        match (level, phase) {
            (Level::Client, Phase::Pre) => self.is_client_pre,
            (Level::Client, Phase::Post) => self.is_client_post,
            (Level::Job, Phase::Pre) => self.is_job_pre,
            (Level::Job, Phase::Post) => self.is_job_post,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Script {
    pub id: ScriptId,
    #[serde(flatten)]
    pub spec: ScriptSpec,
    pub last_updated: DateTime<Utc>,
}

impl Script {
    /// File name of the script inside the uploads directory.
    pub fn file_name(&self) -> String {
        file_name(self.id)
    }
}

pub fn file_name(id: ScriptId) -> String {
    format!("{}.script", id.padded())
}

/// Pre scripts run in name order and post scripts in reverse, so a pair of
/// scripts nests like brackets around the backup.
pub fn sort_for_phase(scripts: &mut [Script], phase: Phase) {
    scripts.sort_by(|a, b| a.spec.name.cmp(&b.spec.name));
    if phase == Phase::Post {
        scripts.reverse();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn script(id: i64, name: &str) -> Script {
        Script {
            id: ScriptId(id),
            spec: ScriptSpec {
                name: name.into(),
                ..ScriptSpec::default()
            },
            last_updated: Utc::now(),
        }
    }

    #[test]
    fn post_scripts_run_in_reverse_name_order() {
        let mut scripts = vec![script(1, "b"), script(2, "a"), script(3, "c")];
        sort_for_phase(&mut scripts, Phase::Pre);
        assert_eq!(scripts.iter().map(|s| s.spec.name.as_str()).collect::<String>(), "abc");
        sort_for_phase(&mut scripts, Phase::Post);
        assert_eq!(scripts.iter().map(|s| s.spec.name.as_str()).collect::<String>(), "cba");
    }

    #[test]
    fn file_name_is_padded() {
        assert_eq!(script(3, "x").file_name(), "0003.script");
    }
}
