//! Clients: machines whose data is backed up.

use serde::{Deserialize, Serialize};

use crate::error::{ValidationErrors, has_line_break};
use crate::ids::{ClientId, ScriptId, UserId};

text_enum! {
    /// Where a client is in its scheduler cycle. Client pre scripts run once
    /// before its first job, post scripts after its last.
    pub enum ClientState {
        Idle => "idle",
        PreScripts => "pre_scripts",
        Ready => "ready",
        PostScripts => "post_scripts",
        Error => "error",
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientSpec {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// rsync/ssh source such as `root@host`; empty for local backups.
    #[serde(default)]
    pub url: String,
    #[serde(default = "yes")]
    pub is_active: bool,
    /// Disk quota in KiB; `None` means unlimited.
    #[serde(default)]
    pub quota_kb: Option<i64>,
    #[serde(default)]
    pub owner_id: Option<UserId>,
    #[serde(default)]
    pub ssh_args: String,
    /// Extra single-letter rsync flags appended to `-a`.
    #[serde(default)]
    pub rsync_short_args: String,
    #[serde(default)]
    pub rsync_long_args: String,
    #[serde(default = "one")]
    pub max_parallel_jobs: u32,
    #[serde(default)]
    pub pre_scripts: Vec<ScriptId>,
    #[serde(default)]
    pub post_scripts: Vec<ScriptId>,
}

fn yes() -> bool {
    true
}

fn one() -> u32 {
    1
}

impl ClientSpec {
    pub fn validate(mut self) -> Result<Self, ValidationErrors> {
        let mut errors = ValidationErrors::new();
        self.name = self.name.trim().to_owned();
        self.url = self.url.trim().to_owned();

        errors.check(!self.name.is_empty(), "name", "name is required");
        errors.check(
            !self.url.contains(char::is_whitespace),
            "url",
            "url must not contain whitespace",
        );
        // A leading dash would make rsync read the source as an option.
        errors.check(!self.url.starts_with('-'), "url", "url must not start with '-'");
        for (field, value) in [("ssh_args", &self.ssh_args), ("rsync_long_args", &self.rsync_long_args)] {
            errors.check(!has_line_break(value), field, "must be a single line");
        }
        errors.check(
            self.rsync_short_args.chars().all(|c| c.is_ascii_alphabetic()),
            "rsync_short_args",
            "only single letter flags are allowed",
        );
        errors.check(
            self.quota_kb.is_none_or(|q| q >= 0),
            "quota_kb",
            "quota must not be negative",
        );
        errors.check(self.max_parallel_jobs >= 1, "max_parallel_jobs", "must be at least 1");
        errors.into_result().map(|()| self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Client {
    pub id: ClientId,
    #[serde(flatten)]
    pub spec: ClientSpec,
    pub state: ClientState,
}

impl Client {
    pub fn is_owned_by(&self, user: UserId) -> bool {
        self.spec.owner_id == Some(user)
    }

    /// Source URL of a job path on this client, in rsync syntax.
    pub fn source_url(&self, path: &str) -> String {
        if self.spec.url.is_empty() {
            path.to_owned()
        } else {
            format!("{}:{path}", self.spec.url)
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn spec(name: &str) -> ClientSpec {
        ClientSpec {
            name: name.into(),
            description: String::new(),
            url: String::new(),
            is_active: true,
            quota_kb: None,
            owner_id: None,
            ssh_args: String::new(),
            rsync_short_args: String::new(),
            rsync_long_args: String::new(),
            max_parallel_jobs: 1,
            pre_scripts: vec![],
            post_scripts: vec![],
        }
    }

    #[test]
    fn source_url_for_local_and_remote() {
        let mut client = Client {
            id: ClientId(1),
            spec: spec("c"),
            state: ClientState::Idle,
        };
        assert_eq!(client.source_url("/etc"), "/etc");
        client.spec.url = "root@host".into();
        assert_eq!(client.source_url("/etc"), "root@host:/etc");
    }

    #[test]
    fn validation_rejects_injection_prone_values() {
        let mut s = spec("c");
        s.url = "root@host\n backup".into();
        s.rsync_short_args = "z -e".into();
        s.ssh_args = "-p 22\nexclude /".into();
        let errors = s.validate().unwrap_err();
        assert_eq!(errors.iter().count(), 3);

        let mut option_like = spec("c");
        option_like.url = "--rsh=sh".into();
        assert!(option_like.validate().is_err());
    }
}
