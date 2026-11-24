use chrono::{DateTime, Utc};
use elkar_core::ids::{ClientId, JobId, UserId};
use elkar_core::log::{LogLevel, LogRecord, NewLogRecord};
use sqlx::{FromRow, PgPool};

use crate::error::{StorageError, StorageResult};

/// Binds the six `LogFilter` parameters used by [`WHERE`].
macro_rules! bind_filter {
    ($query:expr, $filter:expr) => {
        $query
            .bind($filter.min_level.map(LogLevel::as_i16))
            .bind($filter.source.as_deref())
            .bind($filter.client.map(|c| c.0))
            .bind($filter.job.map(|j| j.0))
            .bind($filter.owner.map(|o| o.0))
            .bind($filter.text.as_deref())
    };
}

#[derive(FromRow)]
struct LogRow {
    id: i64,
    created_at: DateTime<Utc>,
    level: i16,
    source: String,
    message: String,
    client_id: Option<i64>,
    job_id: Option<i64>,
    user_name: Option<String>,
    log_file: Option<String>,
}

impl TryFrom<LogRow> for LogRecord {
    type Error = StorageError;

    fn try_from(row: LogRow) -> StorageResult<Self> {
        Ok(Self {
            id: row.id,
            created_at: row.created_at,
            level: LogLevel::from_i16(row.level)
                .ok_or_else(|| StorageError::Corrupt(format!("log level {}", row.level)))?,
            source: row.source,
            message: row.message,
            client_id: row.client_id.map(ClientId),
            job_id: row.job_id.map(JobId),
            user_name: row.user_name,
            log_file: row.log_file,
        })
    }
}

#[derive(Debug, Clone, Default)]
pub struct LogFilter {
    pub min_level: Option<LogLevel>,
    pub source: Option<String>,
    pub client: Option<ClientId>,
    pub job: Option<JobId>,
    /// Restricts to records about clients owned by this user.
    pub owner: Option<UserId>,
    /// Substring of the message.
    pub text: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub total: i64,
    pub page: u32,
    pub per_page: u32,
}

impl<T> Page<T> {
    pub fn page_count(&self) -> u32 {
        let per_page = i64::from(self.per_page.max(1));
        u32::try_from((self.total + per_page - 1) / per_page)
            .unwrap_or(u32::MAX)
            .max(1)
    }
}

const WHERE: &str = "WHERE ($1::SMALLINT IS NULL OR l.level >= $1)
      AND ($2::TEXT IS NULL OR l.source = $2)
      AND ($3::BIGINT IS NULL OR l.client_id = $3)
      AND ($4::BIGINT IS NULL OR l.job_id = $4)
      AND ($5::BIGINT IS NULL OR l.client_id IN (SELECT id FROM clients WHERE owner_id = $5))
      AND ($6::TEXT IS NULL OR l.message ILIKE '%' || $6 || '%')";

pub struct LogRepo<'a> {
    pool: &'a PgPool,
}

impl<'a> LogRepo<'a> {
    pub(crate) fn new(pool: &'a PgPool) -> Self {
        Self { pool }
    }

    pub async fn insert(&self, record: &NewLogRecord) -> StorageResult<i64> {
        Ok(sqlx::query_scalar(
            "INSERT INTO log_records (level, source, message, client_id, job_id, user_name, log_file)
             VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING id",
        )
        .bind(record.level.as_i16())
        .bind(&record.source)
        .bind(&record.message)
        .bind(record.subject.map(|s| s.client_id().0))
        .bind(record.subject.and_then(|s| s.job_id()).map(|j| j.0))
        .bind(&record.user_name)
        .bind(&record.log_file)
        .fetch_one(self.pool)
        .await?)
    }

    pub async fn get(&self, id: i64) -> StorageResult<LogRecord> {
        let row: LogRow = sqlx::query_as("SELECT * FROM log_records WHERE id = $1")
            .bind(id)
            .fetch_one(self.pool)
            .await?;
        row.try_into()
    }

    /// Newest first, `page` starting at 1.
    pub async fn list(&self, filter: &LogFilter, page: u32, per_page: u32) -> StorageResult<Page<LogRecord>> {
        let page = page.max(1);
        let count_sql = format!("SELECT count(*) FROM log_records l {WHERE}");
        let total: i64 = bind_filter!(sqlx::query_scalar(&count_sql), filter)
            .fetch_one(self.pool)
            .await?;
        let list_sql = format!("SELECT l.* FROM log_records l {WHERE} ORDER BY l.id DESC LIMIT $7 OFFSET $8");
        let rows: Vec<LogRow> = bind_filter!(sqlx::query_as(&list_sql), filter)
            .bind(i64::from(per_page))
            .bind(i64::from(page - 1) * i64::from(per_page))
            .fetch_all(self.pool)
            .await?;
        Ok(Page {
            items: rows.into_iter().map(TryInto::try_into).collect::<StorageResult<_>>()?,
            total,
            page,
            per_page,
        })
    }

    /// The latest scheduler status record of each job, for the clients overview.
    pub async fn latest_status_per_job(&self, source: &str) -> StorageResult<Vec<LogRecord>> {
        let rows: Vec<LogRow> = sqlx::query_as(
            "SELECT DISTINCT ON (job_id) * FROM log_records
             WHERE job_id IS NOT NULL AND source = $1
             ORDER BY job_id, id DESC",
        )
        .bind(source)
        .fetch_all(self.pool)
        .await?;
        rows.into_iter().map(TryInto::try_into).collect()
    }

    pub async fn delete_older_than(&self, cutoff: DateTime<Utc>) -> StorageResult<u64> {
        let result = sqlx::query("DELETE FROM log_records WHERE created_at < $1")
            .bind(cutoff)
            .execute(self.pool)
            .await?;
        Ok(result.rows_affected())
    }
}
