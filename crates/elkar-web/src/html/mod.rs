//! Server rendered HTML interface.

mod account;
mod backups;
mod clients;
mod jobs;
mod locations;
mod policies;
mod scripts;
mod system;
mod users;

use askama::Template;
use axum::Router;
use axum::http::header;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::get;
use chrono::{DateTime, Local, Utc};
use elkar_core::ValidationErrors;

use crate::AppState;
use crate::auth::CurrentUser;
use crate::error::{AppError, AppResult};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/", get(|| async { Redirect::to("/clients") }))
        .route("/static/style.css", get(stylesheet))
        .merge(account::routes())
        .merge(clients::routes())
        .merge(jobs::routes())
        .merge(backups::routes())
        .merge(policies::routes())
        .merge(scripts::routes())
        .merge(locations::routes())
        .merge(users::routes())
        .merge(system::routes())
}

async fn stylesheet() -> Response {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        include_str!("../../static/style.css"),
    )
        .into_response()
}

/// Data every page's chrome needs.
pub struct Layout {
    pub user: String,
    pub is_admin: bool,
    pub flash: Option<String>,
    /// Navigation entry to highlight.
    pub active: &'static str,
}

impl Layout {
    pub async fn new(state: &AppState, current: &CurrentUser, active: &'static str) -> Self {
        Self {
            user: current.name().to_owned(),
            is_admin: current.is_admin(),
            flash: current.take_flash(state).await,
            active,
        }
    }
}

pub fn render(template: &impl Template) -> AppResult<Response> {
    Ok(Html(template.render()?).into_response())
}

/// Field errors of a rejected form, ready for display.
#[derive(Default)]
pub struct FormErrors(pub Vec<(String, String)>);

impl FormErrors {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl From<&ValidationErrors> for FormErrors {
    fn from(errors: &ValidationErrors) -> Self {
        Self(
            errors
                .iter()
                .map(|e| (e.field.replace('_', " "), e.message.clone()))
                .collect(),
        )
    }
}

/// Converts a save result into either the new id or the errors to show
/// alongside the re-rendered form. Other errors propagate.
pub fn split_validation<T>(result: AppResult<T>) -> AppResult<Result<T, FormErrors>> {
    match result {
        Ok(value) => Ok(Ok(value)),
        Err(AppError::Invalid(errors)) => Ok(Err(FormErrors::from(&errors))),
        Err(AppError::Conflict(message)) => Ok(Err(FormErrors(vec![("".into(), message)]))),
        Err(other) => Err(other),
    }
}

/// Parsing helpers for text inputs.
pub mod field {
    use elkar_core::ValidationErrors;

    pub fn optional_i64(errors: &mut ValidationErrors, name: &'static str, value: &str) -> Option<i64> {
        let value = value.trim();
        if value.is_empty() {
            return None;
        }
        value
            .parse()
            .map_err(|_| errors.add(name, "must be a whole number"))
            .ok()
    }

    pub fn number<T: std::str::FromStr>(
        errors: &mut ValidationErrors,
        name: &'static str,
        value: &str,
        default: T,
    ) -> T {
        let value = value.trim();
        if value.is_empty() {
            return default;
        }
        value.parse().unwrap_or_else(|_| {
            errors.add(name, "must be a whole number");
            default
        })
    }
}

pub fn format_time(at: DateTime<Utc>) -> String {
    at.with_timezone(&Local).format("%Y-%m-%d %H:%M:%S").to_string()
}

pub fn format_kb(kb: i64) -> String {
    format_bytes(u64::try_from(kb).unwrap_or(0).saturating_mul(1024))
}

pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_are_human_readable() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1536), "1.5 KiB");
        assert_eq!(format_kb(1024 * 1024), "1.0 GiB");
    }
}
