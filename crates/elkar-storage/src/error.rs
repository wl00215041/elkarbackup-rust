#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("record not found")]
    NotFound,
    /// A unique constraint was violated (e.g. duplicate name).
    #[error("already exists: {0}")]
    Conflict(String),
    /// A foreign key prevents the operation (e.g. deleting a used policy).
    #[error("still in use: {0}")]
    InUse(String),
    /// A stored value could not be mapped into the domain model.
    #[error("corrupt data: {0}")]
    Corrupt(String),
    #[error(transparent)]
    Database(sqlx::Error),
}

pub type StorageResult<T> = Result<T, StorageError>;

impl From<sqlx::Error> for StorageError {
    fn from(err: sqlx::Error) -> Self {
        if let sqlx::Error::RowNotFound = err {
            return Self::NotFound;
        }
        if let Some(db) = err.as_database_error() {
            let detail = db.constraint().unwrap_or_else(|| db.message()).to_owned();
            match db.code().as_deref() {
                Some("23505") => return Self::Conflict(detail),
                Some("23503") => return Self::InUse(detail),
                _ => {}
            }
        }
        Self::Database(err)
    }
}

pub(crate) trait OptionalExt<T> {
    /// Turns a missing row into [`StorageError::NotFound`].
    fn found(self) -> StorageResult<T>;
}

impl<T> OptionalExt<T> for Option<T> {
    fn found(self) -> StorageResult<T> {
        self.ok_or(StorageError::NotFound)
    }
}

/// Fails with `NotFound` when an UPDATE/DELETE touched no row.
pub(crate) fn affected(result: sqlx::postgres::PgQueryResult) -> StorageResult<()> {
    if result.rows_affected() == 0 {
        Err(StorageError::NotFound)
    } else {
        Ok(())
    }
}
