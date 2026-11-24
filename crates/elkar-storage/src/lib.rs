//! PostgreSQL persistence for ElkarBackup.
//!
//! [`Store`] is a cheap to clone handle; each entity has a small repository
//! obtained from it (`store.clients()`, `store.jobs()`, ...). Rows are mapped
//! into `elkar-core` types at this boundary so no SQL concern leaks upward.

mod clients;
mod error;
mod jobs;
mod locations;
mod logs;
mod policies;
mod queue;
mod scripts;
mod sessions;
mod users;

use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

pub use clients::{ClientFilter, ClientRepo};
pub use error::{StorageError, StorageResult};
pub use jobs::JobRepo;
pub use locations::LocationRepo;
pub use logs::{LogFilter, LogRepo, Page};
pub use policies::PolicyRepo;
pub use queue::{QueueEntry, QueueRepo};
pub use scripts::ScriptRepo;
pub use sessions::SessionRepo;
pub use users::{UserRepo, UserWithHash};

pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

#[derive(Clone, Debug)]
pub struct Store {
    pool: PgPool,
}

impl Store {
    pub async fn connect(url: &str, max_connections: u32) -> StorageResult<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(max_connections)
            .connect(url)
            .await?;
        Ok(Self { pool })
    }

    pub fn from_pool(pool: PgPool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    pub async fn migrate(&self) -> StorageResult<()> {
        MIGRATOR.run(&self.pool).await.map_err(sqlx::Error::from)?;
        Ok(())
    }

    /// Checks that the database answers.
    pub async fn ping(&self) -> StorageResult<()> {
        sqlx::query("SELECT 1").execute(&self.pool).await?;
        Ok(())
    }

    /// Takes the cluster wide lock identified by `key` for as long as the
    /// returned guard lives. Returns `None` when someone else holds it.
    pub async fn try_lock(&self, key: i64) -> StorageResult<Option<AdvisoryLock>> {
        let mut conn = self.pool.acquire().await?;
        let locked: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
            .bind(key)
            .fetch_one(&mut *conn)
            .await?;
        Ok(locked.then_some(AdvisoryLock { conn }))
    }

    pub fn users(&self) -> UserRepo<'_> {
        UserRepo::new(&self.pool)
    }

    pub fn sessions(&self) -> SessionRepo<'_> {
        SessionRepo::new(&self.pool)
    }

    pub fn locations(&self) -> LocationRepo<'_> {
        LocationRepo::new(&self.pool)
    }

    pub fn policies(&self) -> PolicyRepo<'_> {
        PolicyRepo::new(&self.pool)
    }

    pub fn scripts(&self) -> ScriptRepo<'_> {
        ScriptRepo::new(&self.pool)
    }

    pub fn clients(&self) -> ClientRepo<'_> {
        ClientRepo::new(&self.pool)
    }

    pub fn jobs(&self) -> JobRepo<'_> {
        JobRepo::new(&self.pool)
    }

    pub fn queue(&self) -> QueueRepo<'_> {
        QueueRepo::new(&self.pool)
    }

    pub fn logs(&self) -> LogRepo<'_> {
        LogRepo::new(&self.pool)
    }
}

/// A held PostgreSQL session advisory lock. Session locks belong to the
/// connection, so the guard keeps that connection checked out; dropping it
/// closes the connection, which releases the lock.
pub struct AdvisoryLock {
    conn: sqlx::pool::PoolConnection<sqlx::Postgres>,
}

impl Drop for AdvisoryLock {
    fn drop(&mut self) {
        // Returning the connection to the pool would keep the lock alive.
        self.conn.close_on_drop();
    }
}

/// Helpers shared by the repositories.
mod convert {
    use std::str::FromStr;

    use crate::error::StorageError;

    pub fn parse<T>(value: &str) -> Result<T, StorageError>
    where
        T: FromStr,
        T::Err: std::fmt::Display,
    {
        value.parse().map_err(|e: T::Err| StorageError::Corrupt(e.to_string()))
    }

    pub fn to_u32(value: i32, column: &str) -> Result<u32, StorageError> {
        u32::try_from(value).map_err(|_| StorageError::Corrupt(format!("negative {column}: {value}")))
    }

    pub fn to_i32(value: u32) -> i32 {
        i32::try_from(value).unwrap_or(i32::MAX)
    }
}
