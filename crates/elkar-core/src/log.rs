//! Activity log records shown in the UI and sent in notifications.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::ids::{ClientId, JobId};

/// Severity of an activity record. Numeric values are stored in the
/// database and keep the monolog scale used by the original application.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Debug,
    Info,
    Warning,
    Error,
}

impl LogLevel {
    pub const ALL: &'static [LogLevel] = &[Self::Debug, Self::Info, Self::Warning, Self::Error];

    pub fn as_i16(self) -> i16 {
        match self {
            Self::Debug => 100,
            Self::Info => 200,
            Self::Warning => 300,
            Self::Error => 400,
        }
    }

    pub fn from_i16(value: i16) -> Option<Self> {
        Self::ALL.iter().copied().find(|l| l.as_i16() == value)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }
}

impl std::fmt::Display for LogLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

text_enum! {
    /// Minimum severity a job notification must reach to be emailed.
    pub enum NotificationLevel {
        All => "all",
        Info => "info",
        Warning => "warning",
        Error => "error",
        None => "none",
    }
}

impl NotificationLevel {
    pub fn accepts(self, level: LogLevel) -> bool {
        match self {
            Self::All => true,
            Self::Info => level >= LogLevel::Info,
            Self::Warning => level >= LogLevel::Warning,
            Self::Error => level >= LogLevel::Error,
            Self::None => false,
        }
    }
}

/// The entity a record is about; drives links in the UI and log filtering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Subject {
    Client(ClientId),
    Job(ClientId, JobId),
}

impl Subject {
    pub fn client_id(self) -> ClientId {
        match self {
            Self::Client(c) | Self::Job(c, _) => c,
        }
    }

    pub fn job_id(self) -> Option<JobId> {
        match self {
            Self::Client(_) => None,
            Self::Job(_, j) => Some(j),
        }
    }

    /// Relative URL of the entity's edit page.
    pub fn path(self) -> String {
        match self {
            Self::Client(c) => format!("/client/{c}"),
            Self::Job(c, j) => format!("/client/{c}/job/{j}"),
        }
    }
}

/// A record about to be stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewLogRecord {
    pub level: LogLevel,
    pub source: String,
    pub message: String,
    pub subject: Option<Subject>,
    pub user_name: Option<String>,
    /// File name (inside the job log directory) holding full command output.
    pub log_file: Option<String>,
}

impl NewLogRecord {
    pub fn new(level: LogLevel, source: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            level,
            source: source.into(),
            message: message.into(),
            subject: None,
            user_name: None,
            log_file: None,
        }
    }

    pub fn about(mut self, subject: Subject) -> Self {
        self.subject = Some(subject);
        self
    }

    pub fn by_user(mut self, user_name: impl Into<String>) -> Self {
        self.user_name = Some(user_name.into());
        self
    }

    pub fn with_log_file(mut self, file: impl Into<String>) -> Self {
        self.log_file = Some(file.into());
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LogRecord {
    pub id: i64,
    pub created_at: DateTime<Utc>,
    pub level: LogLevel,
    pub source: String,
    pub message: String,
    pub client_id: Option<ClientId>,
    pub job_id: Option<JobId>,
    pub user_name: Option<String>,
    pub log_file: Option<String>,
}

impl LogRecord {
    pub fn subject(&self) -> Option<Subject> {
        match (self.client_id, self.job_id) {
            (Some(c), Some(j)) => Some(Subject::Job(c, j)),
            (Some(c), None) => Some(Subject::Client(c)),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notification_threshold() {
        assert!(NotificationLevel::All.accepts(LogLevel::Debug));
        assert!(!NotificationLevel::Warning.accepts(LogLevel::Info));
        assert!(NotificationLevel::Warning.accepts(LogLevel::Error));
        assert!(!NotificationLevel::None.accepts(LogLevel::Error));
    }

    #[test]
    fn level_round_trips_through_i16() {
        for level in LogLevel::ALL {
            assert_eq!(LogLevel::from_i16(level.as_i16()), Some(*level));
        }
        assert_eq!(LogLevel::from_i16(1), None);
    }

    #[test]
    fn subject_paths() {
        assert_eq!(Subject::Job(ClientId(1), JobId(2)).path(), "/client/1/job/2");
        assert_eq!(Subject::Client(ClientId(3)).path(), "/client/3");
    }
}
