//! The backup queue: jobs waiting for or undergoing execution.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::ids::{JobId, QueueId};

text_enum! {
    pub enum QueueState {
        Queued => "queued",
        WaitingClient => "waiting_client",
        PreScripts => "pre_scripts",
        Running => "running",
        PostScripts => "post_scripts",
    }
}

impl QueueState {
    /// Whether the item counts against parallelism limits.
    pub fn is_active(self) -> bool {
        self != Self::Queued
    }
}

/// Facts about the last rsnapshot run, exposed to post job scripts.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunStats {
    pub transferred: String,
    pub started_at: i64,
    pub ended_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct QueueItem {
    pub id: QueueId,
    pub job_id: JobId,
    pub enqueued_at: DateTime<Utc>,
    pub running_since: Option<DateTime<Utc>>,
    pub priority: i32,
    pub state: QueueState,
    pub aborted: bool,
}
