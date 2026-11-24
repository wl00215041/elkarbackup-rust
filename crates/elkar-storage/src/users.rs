use elkar_core::ids::UserId;
use elkar_core::user::{Role, User, UserSpec};
use sqlx::{FromRow, PgPool};

use crate::convert::{parse, to_i32, to_u32};
use crate::error::{OptionalExt, StorageResult, affected};

#[derive(FromRow)]
struct UserRow {
    id: i64,
    username: String,
    email: String,
    role: String,
    is_active: bool,
    lines_per_page: i32,
    password_hash: String,
}

impl UserRow {
    fn into_user(self) -> StorageResult<UserWithHash> {
        let user = User {
            id: UserId(self.id),
            spec: UserSpec {
                username: self.username,
                email: self.email,
                role: parse::<Role>(&self.role)?,
                is_active: self.is_active,
                lines_per_page: to_u32(self.lines_per_page, "lines_per_page")?,
            },
        };
        Ok(UserWithHash {
            user,
            password_hash: self.password_hash,
        })
    }
}

/// A user together with its stored password hash, for authentication only.
pub struct UserWithHash {
    pub user: User,
    pub password_hash: String,
}

const COLUMNS: &str = "id, username, email, role, is_active, lines_per_page, password_hash";

pub struct UserRepo<'a> {
    pool: &'a PgPool,
}

impl<'a> UserRepo<'a> {
    pub(crate) fn new(pool: &'a PgPool) -> Self {
        Self { pool }
    }

    pub async fn create(&self, spec: &UserSpec, password_hash: &str) -> StorageResult<UserId> {
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO users (username, email, role, is_active, lines_per_page, password_hash)
             VALUES ($1, $2, $3, $4, $5, $6) RETURNING id",
        )
        .bind(&spec.username)
        .bind(&spec.email)
        .bind(spec.role.as_str())
        .bind(spec.is_active)
        .bind(to_i32(spec.lines_per_page))
        .bind(password_hash)
        .fetch_one(self.pool)
        .await?;
        Ok(UserId(id))
    }

    pub async fn update(&self, id: UserId, spec: &UserSpec) -> StorageResult<()> {
        let result = sqlx::query(
            "UPDATE users SET username = $2, email = $3, role = $4, is_active = $5, lines_per_page = $6
             WHERE id = $1",
        )
        .bind(id.0)
        .bind(&spec.username)
        .bind(&spec.email)
        .bind(spec.role.as_str())
        .bind(spec.is_active)
        .bind(to_i32(spec.lines_per_page))
        .execute(self.pool)
        .await?;
        affected(result)
    }

    pub async fn set_password(&self, id: UserId, password_hash: &str) -> StorageResult<()> {
        let result = sqlx::query("UPDATE users SET password_hash = $2 WHERE id = $1")
            .bind(id.0)
            .bind(password_hash)
            .execute(self.pool)
            .await?;
        affected(result)
    }

    pub async fn get(&self, id: UserId) -> StorageResult<User> {
        let row: Option<UserRow> = sqlx::query_as(&format!("SELECT {COLUMNS} FROM users WHERE id = $1"))
            .bind(id.0)
            .fetch_optional(self.pool)
            .await?;
        Ok(row.found()?.into_user()?.user)
    }

    pub async fn find_by_username(&self, username: &str) -> StorageResult<Option<UserWithHash>> {
        let row: Option<UserRow> = sqlx::query_as(&format!("SELECT {COLUMNS} FROM users WHERE username = $1"))
            .bind(username)
            .fetch_optional(self.pool)
            .await?;
        row.map(UserRow::into_user).transpose()
    }

    pub async fn list(&self) -> StorageResult<Vec<User>> {
        let rows: Vec<UserRow> = sqlx::query_as(&format!("SELECT {COLUMNS} FROM users ORDER BY username"))
            .fetch_all(self.pool)
            .await?;
        rows.into_iter().map(|r| r.into_user().map(|u| u.user)).collect()
    }

    /// Email of the first created admin, used as the system contact.
    pub async fn primary_admin_email(&self) -> StorageResult<Option<String>> {
        Ok(
            sqlx::query_scalar("SELECT email FROM users WHERE role = 'admin' ORDER BY id LIMIT 1")
                .fetch_optional(self.pool)
                .await?,
        )
    }

    pub async fn count(&self) -> StorageResult<i64> {
        Ok(sqlx::query_scalar("SELECT count(*) FROM users")
            .fetch_one(self.pool)
            .await?)
    }

    pub async fn count_active_admins(&self) -> StorageResult<i64> {
        Ok(
            sqlx::query_scalar("SELECT count(*) FROM users WHERE role = 'admin' AND is_active")
                .fetch_one(self.pool)
                .await?,
        )
    }

    pub async fn delete(&self, id: UserId) -> StorageResult<()> {
        let result = sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(id.0)
            .execute(self.pool)
            .await?;
        affected(result)
    }
}
