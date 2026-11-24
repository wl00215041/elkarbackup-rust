use chrono::{DateTime, Utc};
use elkar_core::ids::{ClientId, JobId, ScriptId};
use elkar_core::script::{Phase, Script, ScriptSpec};
use sqlx::{FromRow, PgPool};

use crate::error::{OptionalExt, StorageResult, affected};

#[derive(FromRow)]
struct ScriptRow {
    id: i64,
    name: String,
    description: String,
    is_client_pre: bool,
    is_client_post: bool,
    is_job_pre: bool,
    is_job_post: bool,
    last_updated: DateTime<Utc>,
}

impl From<ScriptRow> for Script {
    fn from(row: ScriptRow) -> Self {
        Self {
            id: ScriptId(row.id),
            spec: ScriptSpec {
                name: row.name,
                description: row.description,
                is_client_pre: row.is_client_pre,
                is_client_post: row.is_client_post,
                is_job_pre: row.is_job_pre,
                is_job_post: row.is_job_post,
            },
            last_updated: row.last_updated,
        }
    }
}

const COLUMNS: &str =
    "s.id, s.name, s.description, s.is_client_pre, s.is_client_post, s.is_job_pre, s.is_job_post, s.last_updated";

pub struct ScriptRepo<'a> {
    pool: &'a PgPool,
}

impl<'a> ScriptRepo<'a> {
    pub(crate) fn new(pool: &'a PgPool) -> Self {
        Self { pool }
    }

    pub async fn create(&self, spec: &ScriptSpec) -> StorageResult<ScriptId> {
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO scripts (name, description, is_client_pre, is_client_post, is_job_pre, is_job_post)
             VALUES ($1, $2, $3, $4, $5, $6) RETURNING id",
        )
        .bind(&spec.name)
        .bind(&spec.description)
        .bind(spec.is_client_pre)
        .bind(spec.is_client_post)
        .bind(spec.is_job_pre)
        .bind(spec.is_job_post)
        .fetch_one(self.pool)
        .await?;
        Ok(ScriptId(id))
    }

    pub async fn update(&self, id: ScriptId, spec: &ScriptSpec) -> StorageResult<()> {
        let result = sqlx::query(
            "UPDATE scripts SET name = $2, description = $3, is_client_pre = $4, is_client_post = $5,
                    is_job_pre = $6, is_job_post = $7, last_updated = now()
             WHERE id = $1",
        )
        .bind(id.0)
        .bind(&spec.name)
        .bind(&spec.description)
        .bind(spec.is_client_pre)
        .bind(spec.is_client_post)
        .bind(spec.is_job_pre)
        .bind(spec.is_job_post)
        .execute(self.pool)
        .await?;
        affected(result)
    }

    pub async fn get(&self, id: ScriptId) -> StorageResult<Script> {
        let row: Option<ScriptRow> = sqlx::query_as(&format!("SELECT {COLUMNS} FROM scripts s WHERE s.id = $1"))
            .bind(id.0)
            .fetch_optional(self.pool)
            .await?;
        Ok(row.found()?.into())
    }

    pub async fn list(&self) -> StorageResult<Vec<Script>> {
        let rows: Vec<ScriptRow> = sqlx::query_as(&format!("SELECT {COLUMNS} FROM scripts s ORDER BY s.name"))
            .fetch_all(self.pool)
            .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// Scripts attached to a client for the given phase.
    pub async fn for_client(&self, client_id: ClientId, phase: Phase) -> StorageResult<Vec<Script>> {
        let rows: Vec<ScriptRow> = sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM scripts s JOIN client_scripts cs ON cs.script_id = s.id
             WHERE cs.client_id = $1 AND cs.phase = $2"
        ))
        .bind(client_id.0)
        .bind(phase.as_str())
        .fetch_all(self.pool)
        .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// Scripts attached to a job for the given phase.
    pub async fn for_job(&self, job_id: JobId, phase: Phase) -> StorageResult<Vec<Script>> {
        let rows: Vec<ScriptRow> = sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM scripts s JOIN job_scripts js ON js.script_id = s.id
             WHERE js.job_id = $1 AND js.phase = $2"
        ))
        .bind(job_id.0)
        .bind(phase.as_str())
        .fetch_all(self.pool)
        .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    pub async fn touch(&self, id: ScriptId) -> StorageResult<()> {
        let result = sqlx::query("UPDATE scripts SET last_updated = now() WHERE id = $1")
            .bind(id.0)
            .execute(self.pool)
            .await?;
        affected(result)
    }

    /// Fails with `InUse` while a client or job references the script.
    pub async fn delete(&self, id: ScriptId) -> StorageResult<()> {
        let result = sqlx::query("DELETE FROM scripts WHERE id = $1")
            .bind(id.0)
            .execute(self.pool)
            .await?;
        affected(result)
    }
}
