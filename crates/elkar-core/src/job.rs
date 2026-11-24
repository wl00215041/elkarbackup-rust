//! Jobs: one backed up path of a client, governed by a policy.

use serde::{Deserialize, Serialize};

use crate::error::{ValidationErrors, has_line_break};
use crate::ids::{ClientId, JobId, LocationId, PolicyId, ScriptId};
use crate::log::NotificationLevel;
use crate::policy::Policy;

text_enum! {
    pub enum Recipient {
        Admin => "admin",
        Owner => "owner",
        Email => "email",
    }
}

text_enum! {
    pub enum BackupResult {
        Ok => "ok",
        Failed => "failed",
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobSpec {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Path on the client to back up.
    pub path: String,
    #[serde(default = "yes")]
    pub is_active: bool,
    pub policy_id: PolicyId,
    pub location_id: LocationId,
    /// Overrides the policy's include patterns when set.
    #[serde(default)]
    pub include: Option<String>,
    /// Overrides the policy's exclude patterns when set.
    #[serde(default)]
    pub exclude: Option<String>,
    #[serde(default = "yes")]
    pub use_local_permissions: bool,
    #[serde(default = "default_recipients")]
    pub notify_to: Vec<Recipient>,
    #[serde(default)]
    pub notification_email: Option<String>,
    #[serde(default = "default_level")]
    pub min_notification_level: NotificationLevel,
    /// Lower runs first.
    #[serde(default = "default_priority")]
    pub priority: i32,
    #[serde(default)]
    pub pre_scripts: Vec<ScriptId>,
    #[serde(default)]
    pub post_scripts: Vec<ScriptId>,
}

fn yes() -> bool {
    true
}

fn default_recipients() -> Vec<Recipient> {
    vec![Recipient::Owner]
}

fn default_level() -> NotificationLevel {
    NotificationLevel::Error
}

fn default_priority() -> i32 {
    i32::MAX
}

impl JobSpec {
    pub fn validate(mut self) -> Result<Self, ValidationErrors> {
        let mut errors = ValidationErrors::new();
        self.name = self.name.trim().to_owned();
        self.path = self.path.trim().to_owned();
        self.include = normalise_patterns(self.include);
        self.exclude = normalise_patterns(self.exclude);
        self.notification_email = self
            .notification_email
            .map(|e| e.trim().to_owned())
            .filter(|e| !e.is_empty());
        self.notify_to.sort_by_key(|r| r.as_str());
        self.notify_to.dedup();

        errors.check(!self.name.is_empty(), "name", "name is required");
        errors.check(
            self.path.starts_with('/') && !self.path.contains(['\t', '\n', '\r']),
            "path",
            "path must be an absolute, single line path",
        );
        if self.notify_to.contains(&Recipient::Email) {
            errors.check(
                self.notification_email.as_deref().is_some_and(looks_like_email),
                "notification_email",
                "a valid email is required when notifying an email address",
            );
        }
        errors.into_result().map(|()| self)
    }
}

fn normalise_patterns(patterns: Option<String>) -> Option<String> {
    patterns
        .map(|p| p.replace("\r\n", "\n").trim().to_owned())
        .filter(|p| !p.is_empty())
}

pub fn looks_like_email(value: &str) -> bool {
    let mut parts = value.split('@');
    matches!(
        (parts.next(), parts.next(), parts.next()),
        (Some(local), Some(domain), None) if !local.is_empty() && domain.contains('.') && !has_line_break(value)
    )
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Job {
    pub id: JobId,
    pub client_id: ClientId,
    #[serde(flatten)]
    pub spec: JobSpec,
    pub disk_usage_kb: i64,
    pub last_result: Option<BackupResult>,
    /// Secret allowing anonymous triggering of the job; `None` disables it.
    #[serde(skip)]
    pub token: Option<String>,
}

impl Job {
    /// Include patterns in effect: the job's own, else the policy's.
    pub fn include<'a>(&'a self, policy: &'a Policy) -> &'a str {
        self.spec.include.as_deref().unwrap_or(&policy.spec.include)
    }

    pub fn exclude<'a>(&'a self, policy: &'a Policy) -> &'a str {
        self.spec.exclude.as_deref().unwrap_or(&policy.spec.exclude)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn spec() -> JobSpec {
        JobSpec {
            name: "etc".into(),
            description: String::new(),
            path: "/etc".into(),
            is_active: true,
            policy_id: PolicyId(1),
            location_id: LocationId(1),
            include: None,
            exclude: None,
            use_local_permissions: true,
            notify_to: default_recipients(),
            notification_email: None,
            min_notification_level: NotificationLevel::Error,
            priority: 10,
            pre_scripts: vec![],
            post_scripts: vec![],
        }
    }

    #[test]
    fn email_recipient_requires_address() {
        let mut s = spec();
        s.notify_to = vec![Recipient::Email, Recipient::Email];
        assert!(s.clone().validate().is_err());
        s.notification_email = Some(" ops@example.com ".into());
        let valid = s.validate().unwrap();
        assert_eq!(valid.notify_to, vec![Recipient::Email]);
        assert_eq!(valid.notification_email.as_deref(), Some("ops@example.com"));
    }

    #[test]
    fn blank_patterns_inherit_from_policy() {
        let mut s = spec();
        s.include = Some("  \r\n".into());
        s.exclude = Some("*.tmp\r\n*.bak".into());
        let valid = s.validate().unwrap();
        assert_eq!(valid.include, None);
        assert_eq!(valid.exclude.as_deref(), Some("*.tmp\n*.bak"));
    }

    #[test]
    fn path_must_be_absolute() {
        for path in ["etc", "--rsh=sh", "/etc\tx"] {
            let mut s = spec();
            s.path = path.into();
            assert!(s.validate().is_err(), "{path:?}");
        }
    }

    #[test]
    fn email_shape() {
        assert!(looks_like_email("a@b.io"));
        assert!(!looks_like_email("a@b"));
        assert!(!looks_like_email("a@@b.io"));
    }
}
