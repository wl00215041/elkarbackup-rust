//! Writing activity records, with an in-memory copy per job execution for
//! notifications.

use elkar_core::log::{LogLevel, NewLogRecord, Subject};
use elkar_storage::Store;

/// Persists activity records. A failure to write a record must never abort
/// a backup, so errors are reported through `tracing` only.
#[derive(Clone)]
pub struct Activity {
    store: Store,
}

impl Activity {
    pub fn new(store: Store) -> Self {
        Self { store }
    }

    pub async fn record(&self, record: NewLogRecord) {
        let subject = record.subject.map(|s| s.path()).unwrap_or_default();
        match record.level {
            LogLevel::Error => tracing::error!(source = %record.source, %subject, "{}", record.message),
            LogLevel::Warning => tracing::warn!(source = %record.source, %subject, "{}", record.message),
            LogLevel::Info => tracing::info!(source = %record.source, %subject, "{}", record.message),
            LogLevel::Debug => tracing::debug!(source = %record.source, %subject, "{}", record.message),
        }
        if let Err(err) = self.store.logs().insert(&record).await {
            tracing::error!(error = %err, "could not store activity record");
        }
    }

    pub fn for_subject(&self, source: &'static str, subject: Subject) -> SubjectLog {
        SubjectLog {
            activity: self.clone(),
            source,
            subject,
            records: Vec::new(),
        }
    }
}

/// Records about one client or job, kept for the notification email.
pub struct SubjectLog {
    activity: Activity,
    source: &'static str,
    subject: Subject,
    records: Vec<NewLogRecord>,
}

impl SubjectLog {
    pub async fn log(&mut self, level: LogLevel, message: impl Into<String>) {
        self.push(NewLogRecord::new(level, self.source, message).about(self.subject))
            .await;
    }

    pub async fn info(&mut self, message: impl Into<String>) {
        self.log(LogLevel::Info, message).await;
    }

    pub async fn warn(&mut self, message: impl Into<String>) {
        self.log(LogLevel::Warning, message).await;
    }

    pub async fn error(&mut self, message: impl Into<String>) {
        self.log(LogLevel::Error, message).await;
    }

    /// Records a fully built record (e.g. with an attached log file).
    pub async fn push(&mut self, record: NewLogRecord) {
        self.activity.record(record.clone()).await;
        self.records.push(record);
    }

    pub fn subject(&self) -> Subject {
        self.subject
    }

    pub fn records(&self) -> &[NewLogRecord] {
        &self.records
    }

    pub fn has_errors(&self) -> bool {
        self.records.iter().any(|r| r.level == LogLevel::Error)
    }
}
