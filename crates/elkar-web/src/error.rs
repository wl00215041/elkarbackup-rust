use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Redirect, Response};
use elkar_core::ValidationErrors;
use elkar_engine::EngineError;
use elkar_storage::StorageError;

/// Errors of request handlers. HTML handlers render them as pages; the API
/// wraps them in [`crate::api::ApiError`] to render JSON.
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("not found")]
    NotFound,
    /// Not logged in.
    #[error("authentication required")]
    Unauthorized,
    #[error("permission denied")]
    Forbidden,
    #[error("{0}")]
    BadRequest(String),
    #[error("invalid input: {0}")]
    Invalid(ValidationErrors),
    #[error("{0}")]
    Conflict(String),
    /// Details are logged, never shown to the client.
    #[error("internal server error")]
    Internal(String),
}

impl AppError {
    pub fn status(&self) -> StatusCode {
        match self {
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::Forbidden => StatusCode::FORBIDDEN,
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::Invalid(_) => StatusCode::UNPROCESSABLE_ENTITY,
            Self::Conflict(_) => StatusCode::CONFLICT,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    pub fn internal(err: impl std::fmt::Display) -> Self {
        Self::Internal(err.to_string())
    }
}

impl From<StorageError> for AppError {
    fn from(err: StorageError) -> Self {
        match err {
            StorageError::NotFound => Self::NotFound,
            StorageError::Conflict(_) => Self::Conflict("an item with that name already exists".into()),
            StorageError::InUse(_) => Self::Conflict("the item is still in use".into()),
            other => Self::internal(other),
        }
    }
}

impl From<EngineError> for AppError {
    fn from(err: EngineError) -> Self {
        match err {
            EngineError::Storage(e) => e.into(),
            EngineError::NotFound(_) => Self::NotFound,
            EngineError::InvalidPath => Self::BadRequest("invalid path".into()),
            other => Self::internal(other),
        }
    }
}

impl From<ValidationErrors> for AppError {
    fn from(errors: ValidationErrors) -> Self {
        Self::Invalid(errors)
    }
}

impl From<askama::Error> for AppError {
    fn from(err: askama::Error) -> Self {
        Self::internal(err)
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        if let Self::Unauthorized = self {
            return Redirect::to("/login").into_response();
        }
        if let Self::Internal(detail) = &self {
            tracing::error!(%detail, "request failed");
        }
        let status = self.status();
        let message = html_escape(&self.to_string());
        let body = format!(
            "<!doctype html><html><head><title>{status}</title><link rel=\"stylesheet\" href=\"/static/style.css\"></head>\
             <body><main class=\"error-page\"><h1>{status}</h1><p>{message}</p><p><a href=\"/\">Back</a></p></main></body></html>"
        );
        (status, Html(body)).into_response()
    }
}

fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

pub type AppResult<T> = Result<T, AppError>;
