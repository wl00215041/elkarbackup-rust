use elkar_storage::StorageError;

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error("{0} not found")]
    NotFound(&'static str),
    #[error("another scheduler is already running")]
    SchedulerBusy,
    #[error("invalid path")]
    InvalidPath,
    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },
    #[error("{0}")]
    Command(String),
}

pub type EngineResult<T> = Result<T, EngineError>;

impl EngineError {
    pub fn io(context: impl Into<String>) -> impl FnOnce(std::io::Error) -> Self {
        let context = context.into();
        move |source| Self::Io { context, source }
    }
}
