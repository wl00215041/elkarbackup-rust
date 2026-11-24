//! Web interface and REST API.
//!
//! * `html` — server rendered pages, authenticated with a session cookie.
//! * `api` — JSON API under `/api`, authenticated with HTTP Basic.
//!
//! Both front ends go through [`access`], which owns authorization and the
//! rules shared by every way of changing data.

mod access;
mod api;
mod auth;
mod error;
mod html;

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use elkar_engine::Engine;

pub use auth::hash_password;

#[derive(Debug, Clone)]
pub struct WebConfig {
    /// Mark cookies `Secure`; enable when served over HTTPS.
    pub secure_cookies: bool,
    pub session_ttl: Duration,
}

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            secure_cookies: false,
            session_ttl: Duration::from_secs(8 * 3600),
        }
    }
}

#[derive(Clone)]
pub struct AppState {
    pub engine: Engine,
    pub config: Arc<WebConfig>,
}

impl AppState {
    pub fn new(engine: Engine, config: WebConfig) -> Self {
        Self {
            engine,
            config: Arc::new(config),
        }
    }

    pub fn store(&self) -> &elkar_storage::Store {
        &self.engine.store
    }
}

/// The complete application router.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(health))
        .merge(html::routes())
        .nest("/api", api::routes())
        .with_state(state)
}

/// Unauthenticated liveness probe; fails while the database is unreachable.
async fn health(State(state): State<AppState>) -> StatusCode {
    match state.store().ping().await {
        Ok(()) => StatusCode::NO_CONTENT,
        Err(err) => {
            tracing::warn!(error = %err, "health check failed");
            StatusCode::SERVICE_UNAVAILABLE
        }
    }
}
