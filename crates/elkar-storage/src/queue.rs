use chrono::{DateTime, Utc};
use elkar_core::ids::{ClientId, JobId, LocationId, QueueId};
use elkar_core::queue::{QueueItem, QueueState, RunStats};
use sqlx::types::Json;
use sqlx::{FromRow, PgPool};

use crate::convert::parse;
use crate::error::StorageResult;

#[derive(FromRow)]
struct QueueRow {
    id: i64,
    job_id: i64,
    enqueued_at: DateTime<Utc>,
    running_since: Option<DateTime<Utc>>,
    priority: i32,
    state: String,
    aborted: bool,
    client_id: i64,
    location_id: i64,
    job_name: String,
    client_name: String,
    active: bool,
}

/// A queue item joined with what the scheduler and status page need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueEntry {
    pub item: QueueItem,
    pub client_id: ClientId,
    pub location_id: LocationId,
    pub job_name: String,
    pub client_name: String,
    /// Both the job and its client are active; inactive entries wait.
    pub active: bool,
}

impl TryFrom<QueueRow> for QueueEntry {
    type Error = crate::StorageError;

    fn try_from(row: QueueRow) -> StorageResult<Self> {
        Ok(Self {
            item: QueueItem {
                id: QueueId(row.id),
                job_id: JobId(row.job_id),
                enqueued_at: row.enqueued_at,
                running_since: row.running_since,
                priority: row.priority,
                state: parse::<QueueState>(&row.state)?,
                aborted: row.aborted,
            },
            client_id: ClientId(row.client_id),
            location_id: LocationId(row.location_id),
            job_name: row.job_name,
            client_name: row.client_name,
            active: row.active,
        })
    }
}

pub struct QueueRepo<'a> {
    pool: &'a PgPool,
}

impl<'a> QueueRepo<'a> {
    pub(crate) fn new(pool: &'a PgPool) -> Self {
        Self { pool }
    }

    /// Adds a job to the queue; returns `false` if it was already queued.
    pub async fn enqueue(&self, job: JobId, at: DateTime<Utc>, priority: i32) -> StorageResult<bool> {
        let result = sqlx::query(
            "INSERT INTO queue (job_id, enqueued_at, priority) VALUES ($1, $2, $3)
             ON CONFLICT (job_id) DO NOTHING",
        )
        .bind(job.0)
        .bind(at)
        .bind(priority)
        .execute(self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    /// Every queue entry in execution order.
    pub async fn list(&self) -> StorageResult<Vec<QueueEntry>> {
        let rows: Vec<QueueRow> = sqlx::query_as(
            "SELECT q.id, q.job_id, q.enqueued_at, q.running_since, q.priority, q.state, q.aborted,
                    j.client_id, j.location_id, j.name AS job_name, c.name AS client_name,
                    (j.is_active AND c.is_active) AS active
             FROM queue q JOIN jobs j ON j.id = q.job_id JOIN clients c ON c.id = j.client_id
             ORDER BY q.enqueued_at, q.priority, q.id",
        )
        .fetch_all(self.pool)
        .await?;
        rows.into_iter().map(TryInto::try_into).collect()
    }

    pub async fn get_by_job(&self, job: JobId) -> StorageResult<Option<QueueItem>> {
        Ok(self
            .list()
            .await?
            .into_iter()
            .find(|e| e.item.job_id == job)
            .map(|e| e.item))
    }

    pub async fn set_state(&self, id: QueueId, state: QueueState) -> StorageResult<()> {
        sqlx::query(
            "UPDATE queue SET state = $2,
                    running_since = CASE WHEN $2 = 'pre_scripts' THEN now() ELSE running_since END
             WHERE id = $1",
        )
        .bind(id.0)
        .bind(state.as_str())
        .execute(self.pool)
        .await?;
        Ok(())
    }

    pub async fn set_run_stats(&self, id: QueueId, stats: &RunStats) -> StorageResult<()> {
        sqlx::query("UPDATE queue SET run_stats = $2 WHERE id = $1")
            .bind(id.0)
            .bind(Json(stats))
            .execute(self.pool)
            .await?;
        Ok(())
    }

    pub async fn run_stats(&self, id: QueueId) -> StorageResult<Option<RunStats>> {
        let stats: Option<Option<Json<RunStats>>> = sqlx::query_scalar("SELECT run_stats FROM queue WHERE id = $1")
            .bind(id.0)
            .fetch_optional(self.pool)
            .await?;
        Ok(stats.flatten().map(|Json(s)| s))
    }

    /// Flags a queued job for abortion; returns `false` if it is not queued.
    pub async fn request_abort(&self, job: JobId) -> StorageResult<bool> {
        let result = sqlx::query("UPDATE queue SET aborted = TRUE, priority = 0 WHERE job_id = $1")
            .bind(job.0)
            .execute(self.pool)
            .await?;
        Ok(result.rows_affected() == 1)
    }

    pub async fn is_aborted(&self, id: QueueId) -> StorageResult<bool> {
        let aborted: Option<bool> = sqlx::query_scalar("SELECT aborted FROM queue WHERE id = $1")
            .bind(id.0)
            .fetch_optional(self.pool)
            .await?;
        Ok(aborted.unwrap_or(true))
    }

    pub async fn remove(&self, id: QueueId) -> StorageResult<()> {
        sqlx::query("DELETE FROM queue WHERE id = $1")
            .bind(id.0)
            .execute(self.pool)
            .await?;
        Ok(())
    }

    /// Returns items left in flight by a previous scheduler to `queued`.
    pub async fn reset_in_flight(&self) -> StorageResult<Vec<JobId>> {
        let ids: Vec<i64> = sqlx::query_scalar(
            "UPDATE queue SET state = 'queued', running_since = NULL WHERE state <> 'queued' RETURNING job_id",
        )
        .fetch_all(self.pool)
        .await?;
        Ok(ids.into_iter().map(JobId).collect())
    }
}
