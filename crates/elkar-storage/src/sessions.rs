use chrono::{DateTime, Utc};
use elkar_core::ids::UserId;
use sqlx::PgPool;

use crate::error::StorageResult;

/// Login sessions. Only a hash of the cookie token is stored, so a database
/// leak does not hand out valid sessions.
pub struct SessionRepo<'a> {
    pool: &'a PgPool,
}

impl<'a> SessionRepo<'a> {
    pub(crate) fn new(pool: &'a PgPool) -> Self {
        Self { pool }
    }

    pub async fn create(&self, token_hash: &str, user: UserId, expires_at: DateTime<Utc>) -> StorageResult<()> {
        sqlx::query("INSERT INTO sessions (token_hash, user_id, expires_at) VALUES ($1, $2, $3)")
            .bind(token_hash)
            .bind(user.0)
            .bind(expires_at)
            .execute(self.pool)
            .await?;
        Ok(())
    }

    /// The active user owning a live session.
    pub async fn user_for(&self, token_hash: &str) -> StorageResult<Option<UserId>> {
        let id: Option<i64> = sqlx::query_scalar(
            "SELECT s.user_id FROM sessions s JOIN users u ON u.id = s.user_id
             WHERE s.token_hash = $1 AND s.expires_at > now() AND u.is_active",
        )
        .bind(token_hash)
        .fetch_optional(self.pool)
        .await?;
        Ok(id.map(UserId))
    }

    pub async fn set_flash(&self, token_hash: &str, message: &str) -> StorageResult<()> {
        sqlx::query("UPDATE sessions SET flash = $2 WHERE token_hash = $1")
            .bind(token_hash)
            .bind(message)
            .execute(self.pool)
            .await?;
        Ok(())
    }

    /// Returns and clears the pending flash message.
    pub async fn take_flash(&self, token_hash: &str) -> StorageResult<Option<String>> {
        let flash: Option<Option<String>> = sqlx::query_scalar(
            "UPDATE sessions s SET flash = NULL FROM sessions old
             WHERE s.token_hash = $1 AND old.token_hash = s.token_hash AND old.flash IS NOT NULL
             RETURNING old.flash",
        )
        .bind(token_hash)
        .fetch_optional(self.pool)
        .await?;
        Ok(flash.flatten())
    }

    pub async fn delete(&self, token_hash: &str) -> StorageResult<()> {
        sqlx::query("DELETE FROM sessions WHERE token_hash = $1")
            .bind(token_hash)
            .execute(self.pool)
            .await?;
        Ok(())
    }

    pub async fn delete_for_user(&self, user: UserId) -> StorageResult<()> {
        sqlx::query("DELETE FROM sessions WHERE user_id = $1")
            .bind(user.0)
            .execute(self.pool)
            .await?;
        Ok(())
    }

    pub async fn purge_expired(&self) -> StorageResult<u64> {
        let result = sqlx::query("DELETE FROM sessions WHERE expires_at <= now()")
            .execute(self.pool)
            .await?;
        Ok(result.rows_affected())
    }
}
