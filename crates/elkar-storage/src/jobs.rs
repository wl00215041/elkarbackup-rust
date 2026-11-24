use elkar_core::ids::{ClientId, JobId, LocationId, PolicyId, ScriptId};
use elkar_core::job::{BackupResult, Job, JobSpec, Recipient};
use elkar_core::log::NotificationLevel;
use elkar_core::script::Phase;
use sqlx::{FromRow, PgPool, Postgres, Transaction};

use crate::convert::parse;
use crate::error::{OptionalExt, StorageResult, affected};

#[derive(FromRow)]
struct JobRow {
    id: i64,
    client_id: i64,
    name: String,
    description: String,
    path: String,
    is_active: bool,
    policy_id: i64,
    location_id: i64,
    include: Option<String>,
    exclude: Option<String>,
    use_local_permissions: bool,
    notify_to: Vec<String>,
    notification_email: Option<String>,
    min_notification_level: String,
    priority: i32,
    disk_usage_kb: i64,
    last_result: Option<String>,
    token: Option<String>,
    pre_scripts: Vec<i64>,
    post_scripts: Vec<i64>,
}

impl TryFrom<JobRow> for Job {
    type Error = crate::StorageError;

    fn try_from(row: JobRow) -> StorageResult<Self> {
        Ok(Self {
            id: JobId(row.id),
            client_id: ClientId(row.client_id),
            spec: JobSpec {
                name: row.name,
                description: row.description,
                path: row.path,
                is_active: row.is_active,
                policy_id: PolicyId(row.policy_id),
                location_id: LocationId(row.location_id),
                include: row.include,
                exclude: row.exclude,
                use_local_permissions: row.use_local_permissions,
                notify_to: row
                    .notify_to
                    .iter()
                    .map(|r| parse::<Recipient>(r))
                    .collect::<StorageResult<_>>()?,
                notification_email: row.notification_email,
                min_notification_level: parse::<NotificationLevel>(&row.min_notification_level)?,
                priority: row.priority,
                pre_scripts: row.pre_scripts.into_iter().map(ScriptId).collect(),
                post_scripts: row.post_scripts.into_iter().map(ScriptId).collect(),
            },
            disk_usage_kb: row.disk_usage_kb,
            last_result: row.last_result.as_deref().map(parse::<BackupResult>).transpose()?,
            token: row.token,
        })
    }
}

const SELECT: &str = "SELECT j.*,
    ARRAY(SELECT script_id FROM job_scripts WHERE job_id = j.id AND phase = 'PRE' ORDER BY script_id) AS pre_scripts,
    ARRAY(SELECT script_id FROM job_scripts WHERE job_id = j.id AND phase = 'POST' ORDER BY script_id) AS post_scripts
    FROM jobs j";

pub struct JobRepo<'a> {
    pool: &'a PgPool,
}

impl<'a> JobRepo<'a> {
    pub(crate) fn new(pool: &'a PgPool) -> Self {
        Self { pool }
    }

    pub async fn create(&self, client: ClientId, spec: &JobSpec) -> StorageResult<JobId> {
        let mut tx = self.pool.begin().await?;
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO jobs (client_id, name, description, path, is_active, policy_id, location_id,
                               include, exclude, use_local_permissions, notify_to, notification_email,
                               min_notification_level, priority)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14) RETURNING id",
        )
        .bind(client.0)
        .bind(&spec.name)
        .bind(&spec.description)
        .bind(&spec.path)
        .bind(spec.is_active)
        .bind(spec.policy_id.0)
        .bind(spec.location_id.0)
        .bind(&spec.include)
        .bind(&spec.exclude)
        .bind(spec.use_local_permissions)
        .bind(recipients(spec))
        .bind(&spec.notification_email)
        .bind(spec.min_notification_level.as_str())
        .bind(spec.priority)
        .fetch_one(&mut *tx)
        .await?;
        replace_scripts(&mut tx, id, spec).await?;
        tx.commit().await?;
        Ok(JobId(id))
    }

    pub async fn update(&self, id: JobId, spec: &JobSpec) -> StorageResult<()> {
        let mut tx = self.pool.begin().await?;
        let result = sqlx::query(
            "UPDATE jobs SET name = $2, description = $3, path = $4, is_active = $5, policy_id = $6,
                    location_id = $7, include = $8, exclude = $9, use_local_permissions = $10,
                    notify_to = $11, notification_email = $12, min_notification_level = $13, priority = $14
             WHERE id = $1",
        )
        .bind(id.0)
        .bind(&spec.name)
        .bind(&spec.description)
        .bind(&spec.path)
        .bind(spec.is_active)
        .bind(spec.policy_id.0)
        .bind(spec.location_id.0)
        .bind(&spec.include)
        .bind(&spec.exclude)
        .bind(spec.use_local_permissions)
        .bind(recipients(spec))
        .bind(&spec.notification_email)
        .bind(spec.min_notification_level.as_str())
        .bind(spec.priority)
        .execute(&mut *tx)
        .await?;
        affected(result)?;
        replace_scripts(&mut tx, id.0, spec).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn get(&self, id: JobId) -> StorageResult<Job> {
        let row: Option<JobRow> = sqlx::query_as(&format!("{SELECT} WHERE j.id = $1"))
            .bind(id.0)
            .fetch_optional(self.pool)
            .await?;
        row.found()?.try_into()
    }

    pub async fn list(&self) -> StorageResult<Vec<Job>> {
        self.fetch(&format!("{SELECT} ORDER BY j.client_id, j.priority, j.name"), None)
            .await
    }

    pub async fn list_for_client(&self, client: ClientId) -> StorageResult<Vec<Job>> {
        self.fetch(
            &format!("{SELECT} WHERE j.client_id = $1 ORDER BY j.priority, j.name"),
            Some(client.0),
        )
        .await
    }

    /// Active jobs of active clients governed by one of `policies`, in the
    /// order they should be enqueued.
    pub async fn list_schedulable(&self, policies: &[PolicyId]) -> StorageResult<Vec<Job>> {
        let ids: Vec<i64> = policies.iter().map(|p| p.0).collect();
        let rows: Vec<JobRow> = sqlx::query_as(&format!(
            "{SELECT} JOIN clients c ON c.id = j.client_id
             WHERE j.is_active AND c.is_active AND j.policy_id = ANY($1)
             ORDER BY j.priority, c.id"
        ))
        .bind(&ids)
        .fetch_all(self.pool)
        .await?;
        rows.into_iter().map(TryInto::try_into).collect()
    }

    async fn fetch(&self, sql: &str, client: Option<i64>) -> StorageResult<Vec<Job>> {
        let mut query = sqlx::query_as::<_, JobRow>(sql);
        if let Some(client) = client {
            query = query.bind(client);
        }
        let rows = query.fetch_all(self.pool).await?;
        rows.into_iter().map(TryInto::try_into).collect()
    }

    pub async fn delete(&self, id: JobId) -> StorageResult<()> {
        let result = sqlx::query("DELETE FROM jobs WHERE id = $1")
            .bind(id.0)
            .execute(self.pool)
            .await?;
        affected(result)
    }

    pub async fn set_last_result(&self, id: JobId, result: BackupResult) -> StorageResult<()> {
        sqlx::query("UPDATE jobs SET last_result = $2 WHERE id = $1")
            .bind(id.0)
            .bind(result.as_str())
            .execute(self.pool)
            .await?;
        Ok(())
    }

    pub async fn set_disk_usage(&self, id: JobId, kb: i64) -> StorageResult<()> {
        sqlx::query("UPDATE jobs SET disk_usage_kb = $2 WHERE id = $1")
            .bind(id.0)
            .bind(kb)
            .execute(self.pool)
            .await?;
        Ok(())
    }

    /// Sets or clears the anonymous trigger token.
    pub async fn set_token(&self, id: JobId, token: Option<&str>) -> StorageResult<()> {
        let result = sqlx::query("UPDATE jobs SET token = $2 WHERE id = $1")
            .bind(id.0)
            .bind(token)
            .execute(self.pool)
            .await?;
        affected(result)
    }

    /// Re-orders jobs: position in `ordered` becomes the priority.
    pub async fn set_priorities(&self, ordered: &[JobId]) -> StorageResult<()> {
        let mut tx = self.pool.begin().await?;
        for (priority, id) in (0_i32..).zip(ordered) {
            sqlx::query("UPDATE jobs SET priority = $2 WHERE id = $1")
                .bind(id.0)
                .bind(priority)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }
}

fn recipients(spec: &JobSpec) -> Vec<&'static str> {
    spec.notify_to.iter().map(|r| r.as_str()).collect()
}

async fn replace_scripts(tx: &mut Transaction<'_, Postgres>, job_id: i64, spec: &JobSpec) -> StorageResult<()> {
    sqlx::query("DELETE FROM job_scripts WHERE job_id = $1")
        .bind(job_id)
        .execute(&mut **tx)
        .await?;
    for (phase, scripts) in [(Phase::Pre, &spec.pre_scripts), (Phase::Post, &spec.post_scripts)] {
        let ids: Vec<i64> = scripts.iter().map(|s| s.0).collect();
        sqlx::query(
            "INSERT INTO job_scripts (job_id, script_id, phase)
             SELECT $1, unnest($2::BIGINT[]), $3 ON CONFLICT DO NOTHING",
        )
        .bind(job_id)
        .bind(&ids)
        .bind(phase.as_str())
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}
