use chrono::{DateTime, Utc};
use elkar_core::client::{Client, ClientSpec, ClientState};
use elkar_core::ids::{ClientId, ScriptId, UserId};
use elkar_core::script::Phase;
use sqlx::{FromRow, PgPool, Postgres, Transaction};

use crate::convert::{parse, to_i32, to_u32};
use crate::error::{OptionalExt, StorageResult, affected};

#[derive(FromRow)]
struct ClientRow {
    id: i64,
    name: String,
    description: String,
    url: String,
    is_active: bool,
    quota_kb: Option<i64>,
    owner_id: Option<i64>,
    ssh_args: String,
    rsync_short_args: String,
    rsync_long_args: String,
    max_parallel_jobs: i32,
    state: String,
    pre_scripts: Vec<i64>,
    post_scripts: Vec<i64>,
}

impl TryFrom<ClientRow> for Client {
    type Error = crate::StorageError;

    fn try_from(row: ClientRow) -> StorageResult<Self> {
        Ok(Self {
            id: ClientId(row.id),
            state: parse::<ClientState>(&row.state)?,
            spec: ClientSpec {
                name: row.name,
                description: row.description,
                url: row.url,
                is_active: row.is_active,
                quota_kb: row.quota_kb,
                owner_id: row.owner_id.map(UserId),
                ssh_args: row.ssh_args,
                rsync_short_args: row.rsync_short_args,
                rsync_long_args: row.rsync_long_args,
                max_parallel_jobs: to_u32(row.max_parallel_jobs, "max_parallel_jobs")?,
                pre_scripts: row.pre_scripts.into_iter().map(ScriptId).collect(),
                post_scripts: row.post_scripts.into_iter().map(ScriptId).collect(),
            },
        })
    }
}

const SELECT: &str = "SELECT c.*,
    ARRAY(SELECT script_id FROM client_scripts WHERE client_id = c.id AND phase = 'PRE' ORDER BY script_id) AS pre_scripts,
    ARRAY(SELECT script_id FROM client_scripts WHERE client_id = c.id AND phase = 'POST' ORDER BY script_id) AS post_scripts
    FROM clients c";

/// Restricts a client listing.
#[derive(Debug, Clone, Default)]
pub struct ClientFilter {
    /// Only clients owned by this user (non admin views).
    pub owner: Option<UserId>,
    /// Case insensitive substring of the name.
    pub name: Option<String>,
}

pub struct ClientRepo<'a> {
    pool: &'a PgPool,
}

impl<'a> ClientRepo<'a> {
    pub(crate) fn new(pool: &'a PgPool) -> Self {
        Self { pool }
    }

    pub async fn create(&self, spec: &ClientSpec) -> StorageResult<ClientId> {
        let mut tx = self.pool.begin().await?;
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO clients (name, description, url, is_active, quota_kb, owner_id, ssh_args,
                                  rsync_short_args, rsync_long_args, max_parallel_jobs)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) RETURNING id",
        )
        .bind(&spec.name)
        .bind(&spec.description)
        .bind(&spec.url)
        .bind(spec.is_active)
        .bind(spec.quota_kb)
        .bind(spec.owner_id.map(|o| o.0))
        .bind(&spec.ssh_args)
        .bind(&spec.rsync_short_args)
        .bind(&spec.rsync_long_args)
        .bind(to_i32(spec.max_parallel_jobs))
        .fetch_one(&mut *tx)
        .await?;
        replace_scripts(&mut tx, id, spec).await?;
        tx.commit().await?;
        Ok(ClientId(id))
    }

    pub async fn update(&self, id: ClientId, spec: &ClientSpec) -> StorageResult<()> {
        let mut tx = self.pool.begin().await?;
        let result = sqlx::query(
            "UPDATE clients SET name = $2, description = $3, url = $4, is_active = $5, quota_kb = $6,
                    owner_id = $7, ssh_args = $8, rsync_short_args = $9, rsync_long_args = $10,
                    max_parallel_jobs = $11
             WHERE id = $1",
        )
        .bind(id.0)
        .bind(&spec.name)
        .bind(&spec.description)
        .bind(&spec.url)
        .bind(spec.is_active)
        .bind(spec.quota_kb)
        .bind(spec.owner_id.map(|o| o.0))
        .bind(&spec.ssh_args)
        .bind(&spec.rsync_short_args)
        .bind(&spec.rsync_long_args)
        .bind(to_i32(spec.max_parallel_jobs))
        .execute(&mut *tx)
        .await?;
        affected(result)?;
        replace_scripts(&mut tx, id.0, spec).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn get(&self, id: ClientId) -> StorageResult<Client> {
        let row: Option<ClientRow> = sqlx::query_as(&format!("{SELECT} WHERE c.id = $1"))
            .bind(id.0)
            .fetch_optional(self.pool)
            .await?;
        row.found()?.try_into()
    }

    pub async fn list(&self, filter: &ClientFilter) -> StorageResult<Vec<Client>> {
        let rows: Vec<ClientRow> = sqlx::query_as(&format!(
            "{SELECT} WHERE ($1::BIGINT IS NULL OR c.owner_id = $1)
               AND ($2::TEXT IS NULL OR c.name ILIKE '%' || $2 || '%')
             ORDER BY c.name"
        ))
        .bind(filter.owner.map(|o| o.0))
        .bind(filter.name.as_deref())
        .fetch_all(self.pool)
        .await?;
        rows.into_iter().map(TryInto::try_into).collect()
    }

    pub async fn delete(&self, id: ClientId) -> StorageResult<()> {
        let result = sqlx::query("DELETE FROM clients WHERE id = $1")
            .bind(id.0)
            .execute(self.pool)
            .await?;
        affected(result)
    }

    pub async fn set_state(&self, id: ClientId, state: ClientState) -> StorageResult<()> {
        let result = sqlx::query("UPDATE clients SET state = $2 WHERE id = $1")
            .bind(id.0)
            .bind(state.as_str())
            .execute(self.pool)
            .await?;
        affected(result)
    }

    /// Records when the client's backup cycle began (`None` clears it).
    pub async fn set_cycle_started(&self, id: ClientId, at: Option<DateTime<Utc>>) -> StorageResult<()> {
        sqlx::query("UPDATE clients SET cycle_started_at = $2 WHERE id = $1")
            .bind(id.0)
            .bind(at)
            .execute(self.pool)
            .await?;
        Ok(())
    }

    pub async fn cycle_started(&self, id: ClientId) -> StorageResult<Option<DateTime<Utc>>> {
        Ok(sqlx::query_scalar("SELECT cycle_started_at FROM clients WHERE id = $1")
            .bind(id.0)
            .fetch_one(self.pool)
            .await?)
    }

    /// Puts every client back to `idle`, used when the scheduler (re)starts.
    pub async fn reset_states(&self) -> StorageResult<()> {
        sqlx::query("UPDATE clients SET state = 'idle' WHERE state <> 'idle'")
            .execute(self.pool)
            .await?;
        Ok(())
    }

    /// Total disk usage of every job of the client, in KiB.
    pub async fn disk_usage_kb(&self, id: ClientId) -> StorageResult<i64> {
        Ok(
            sqlx::query_scalar("SELECT COALESCE(sum(disk_usage_kb), 0)::BIGINT FROM jobs WHERE client_id = $1")
                .bind(id.0)
                .fetch_one(self.pool)
                .await?,
        )
    }
}

async fn replace_scripts(tx: &mut Transaction<'_, Postgres>, client_id: i64, spec: &ClientSpec) -> StorageResult<()> {
    sqlx::query("DELETE FROM client_scripts WHERE client_id = $1")
        .bind(client_id)
        .execute(&mut **tx)
        .await?;
    for (phase, scripts) in [(Phase::Pre, &spec.pre_scripts), (Phase::Post, &spec.post_scripts)] {
        let ids: Vec<i64> = scripts.iter().map(|s| s.0).collect();
        sqlx::query(
            "INSERT INTO client_scripts (client_id, script_id, phase)
             SELECT $1, unnest($2::BIGINT[]), $3 ON CONFLICT DO NOTHING",
        )
        .bind(client_id)
        .bind(&ids)
        .bind(phase.as_str())
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}
