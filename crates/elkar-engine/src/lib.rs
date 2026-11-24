//! Backup execution and scheduling.
//!
//! The [`Scheduler`] drives queued jobs through their life cycle; every
//! external program (rsnapshot, user scripts, rsync, du) is started through
//! the [`ProcessRunner`] trait so the state machine is testable without them.

pub mod activity;
pub mod backup;
pub mod config;
pub mod error;
mod execution;
pub mod keys;
pub mod notify;
pub mod process;
pub mod restore;
pub mod scheduler;
pub mod scripts;
pub mod snapshots;

use std::sync::Arc;

use chrono::Utc;
use elkar_core::ids::{ClientId, JobId};
use elkar_core::log::{LogLevel, NewLogRecord, Subject};
use elkar_storage::Store;

pub use activity::Activity;
pub use config::EngineConfig;
pub use error::{EngineError, EngineResult};
pub use notify::{Notifier, NullNotifier, SmtpNotifier};
pub use process::{ProcessRunner, SystemRunner};
pub use scheduler::Scheduler;

/// Shared dependencies of every engine component.
#[derive(Clone)]
pub struct Engine {
    pub store: Store,
    pub config: Arc<EngineConfig>,
    pub runner: Arc<dyn ProcessRunner>,
    pub notifier: Arc<dyn Notifier>,
}

/// Source name used for scheduler status records; the clients overview
/// shows the latest of these per job.
pub const STATUS_SOURCE: &str = "Scheduler";

/// Outcome of a manual enqueue request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnqueueOutcome {
    Queued,
    AlreadyQueued,
}

impl Engine {
    pub fn activity(&self) -> Activity {
        Activity::new(self.store.clone())
    }

    /// Queues a job on demand; it runs the policy's lowest retain level.
    pub async fn enqueue_now(&self, client: ClientId, job: JobId, by: Option<&str>) -> EngineResult<EnqueueOutcome> {
        let loaded = self.store.jobs().get(job).await?;
        if loaded.client_id != client {
            return Err(EngineError::NotFound("job"));
        }
        let queued = self
            .store
            .queue()
            .enqueue(job, Utc::now(), loaded.spec.priority)
            .await?;
        let (level, message, outcome) = if queued {
            (LogLevel::Info, "Job queued", EnqueueOutcome::Queued)
        } else {
            (
                LogLevel::Warning,
                "Job already queued, not queued again",
                EnqueueOutcome::AlreadyQueued,
            )
        };
        let mut record = NewLogRecord::new(level, STATUS_SOURCE, message).about(Subject::Job(client, job));
        if let Some(user) = by {
            record = record.by_user(user);
        }
        self.activity().record(record).await;
        Ok(outcome)
    }

    /// Requests a queued or running job to stop; returns `false` if the job
    /// is not in the queue.
    pub async fn request_abort(&self, client: ClientId, job: JobId, by: &str) -> EngineResult<bool> {
        let aborted = self.store.queue().request_abort(job).await?;
        if aborted {
            self.activity()
                .record(
                    NewLogRecord::new(LogLevel::Warning, STATUS_SOURCE, "Job stop requested")
                        .about(Subject::Job(client, job))
                        .by_user(by),
                )
                .await;
        }
        Ok(aborted)
    }
}
