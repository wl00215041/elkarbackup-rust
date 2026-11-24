use elkar_core::ids::LocationId;
use elkar_core::location::{BackupLocation, LocationSpec};
use sqlx::{FromRow, PgPool};

use crate::convert::{to_i32, to_u32};
use crate::error::{OptionalExt, StorageResult, affected};

#[derive(FromRow)]
struct LocationRow {
    id: i64,
    name: String,
    host: Option<String>,
    directory: String,
    max_parallel_jobs: i32,
}

impl TryFrom<LocationRow> for BackupLocation {
    type Error = crate::StorageError;

    fn try_from(row: LocationRow) -> StorageResult<Self> {
        Ok(Self {
            id: LocationId(row.id),
            spec: LocationSpec {
                name: row.name,
                host: row.host,
                directory: row.directory,
                max_parallel_jobs: to_u32(row.max_parallel_jobs, "max_parallel_jobs")?,
            },
        })
    }
}

pub struct LocationRepo<'a> {
    pool: &'a PgPool,
}

impl<'a> LocationRepo<'a> {
    pub(crate) fn new(pool: &'a PgPool) -> Self {
        Self { pool }
    }

    pub async fn create(&self, spec: &LocationSpec) -> StorageResult<LocationId> {
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO backup_locations (name, host, directory, max_parallel_jobs)
             VALUES ($1, $2, $3, $4) RETURNING id",
        )
        .bind(&spec.name)
        .bind(&spec.host)
        .bind(&spec.directory)
        .bind(to_i32(spec.max_parallel_jobs))
        .fetch_one(self.pool)
        .await?;
        Ok(LocationId(id))
    }

    pub async fn update(&self, id: LocationId, spec: &LocationSpec) -> StorageResult<()> {
        let result = sqlx::query(
            "UPDATE backup_locations SET name = $2, host = $3, directory = $4, max_parallel_jobs = $5
             WHERE id = $1",
        )
        .bind(id.0)
        .bind(&spec.name)
        .bind(&spec.host)
        .bind(&spec.directory)
        .bind(to_i32(spec.max_parallel_jobs))
        .execute(self.pool)
        .await?;
        affected(result)
    }

    pub async fn get(&self, id: LocationId) -> StorageResult<BackupLocation> {
        let row: Option<LocationRow> = sqlx::query_as("SELECT * FROM backup_locations WHERE id = $1")
            .bind(id.0)
            .fetch_optional(self.pool)
            .await?;
        row.found()?.try_into()
    }

    pub async fn list(&self) -> StorageResult<Vec<BackupLocation>> {
        let rows: Vec<LocationRow> = sqlx::query_as("SELECT * FROM backup_locations ORDER BY name")
            .fetch_all(self.pool)
            .await?;
        rows.into_iter().map(TryInto::try_into).collect()
    }

    pub async fn delete(&self, id: LocationId) -> StorageResult<()> {
        let result = sqlx::query("DELETE FROM backup_locations WHERE id = $1")
            .bind(id.0)
            .execute(self.pool)
            .await?;
        affected(result)
    }
}
