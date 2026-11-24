//! Logs, queue status and server information.

use askama::Template;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::header;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use elkar_core::ids::{ClientId, JobId};
use elkar_core::log::LogLevel;
use elkar_engine::{keys, snapshots};
use elkar_storage::LogFilter;
use serde::Deserialize;

use super::{Layout, format_bytes, format_time, render};
use crate::AppState;
use crate::access::Access;
use crate::auth::CurrentUser;
use crate::error::{AppError, AppResult};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/logs", get(logs))
        .route("/log/{id}/download", get(download_log))
        .route("/status", get(status))
        .route("/about", get(about))
        .route("/config/publickey", get(public_key))
        .route("/config/publickey/generate", post(generate_key))
}

#[derive(Deserialize, Default)]
struct LogQuery {
    #[serde(default)]
    level: String,
    #[serde(default)]
    source: String,
    client: Option<i64>,
    job: Option<i64>,
    #[serde(default)]
    text: String,
    page: Option<u32>,
}

impl LogQuery {
    /// Query string for another page with the same filters.
    fn link(&self, page: u32) -> String {
        let mut parts = vec![format!("page={page}")];
        for (key, value) in [("level", &self.level), ("source", &self.source), ("text", &self.text)] {
            if !value.is_empty() {
                parts.push(format!("{key}={}", url_encode(value)));
            }
        }
        if let Some(c) = self.client {
            parts.push(format!("client={c}"));
        }
        if let Some(j) = self.job {
            parts.push(format!("job={j}"));
        }
        format!("/logs?{}", parts.join("&"))
    }
}

fn url_encode(value: &str) -> String {
    value
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

struct LogRow {
    id: i64,
    time: String,
    level: &'static str,
    source: String,
    subject: Option<String>,
    message: String,
    user: String,
    has_file: bool,
}

#[derive(Template)]
#[template(path = "logs.html")]
struct LogsPage {
    layout: Layout,
    query: LogQuery,
    levels: Vec<&'static str>,
    rows: Vec<LogRow>,
    total: i64,
    page: u32,
    pages: u32,
    prev: Option<String>,
    next: Option<String>,
}

async fn logs(
    State(state): State<AppState>,
    current: CurrentUser,
    Query(query): Query<LogQuery>,
) -> AppResult<Response> {
    let access = Access::new(&state, &current.user);
    let filter = access.log_filter(LogFilter {
        min_level: LogLevel::ALL.iter().copied().find(|l| l.as_str() == query.level),
        source: Some(query.source.clone()).filter(|s| !s.is_empty()),
        client: query.client.map(ClientId),
        job: query.job.map(JobId),
        owner: None,
        text: Some(query.text.clone()).filter(|s| !s.is_empty()),
    });
    let page = state
        .store()
        .logs()
        .list(&filter, query.page.unwrap_or(1), current.user.spec.lines_per_page)
        .await?;
    let pages = page.page_count();
    let rows = page
        .items
        .into_iter()
        .map(|r| LogRow {
            id: r.id,
            time: format_time(r.created_at),
            level: r.level.as_str(),
            subject: r.subject().map(|s| s.path()),
            has_file: r.log_file.is_some(),
            source: r.source,
            message: r.message,
            user: r.user_name.unwrap_or_default(),
        })
        .collect();
    render(&LogsPage {
        layout: Layout::new(&state, &current, "logs").await,
        levels: LogLevel::ALL.iter().map(|l| l.as_str()).collect(),
        rows,
        total: page.total,
        page: page.page,
        pages,
        prev: (page.page > 1).then(|| query.link(page.page - 1)),
        next: (page.page < pages).then(|| query.link(page.page + 1)),
        query,
    })
}

/// Full output of a failed rsnapshot run attached to a log record.
async fn download_log(State(state): State<AppState>, current: CurrentUser, Path(id): Path<i64>) -> AppResult<Response> {
    let record = state.store().logs().get(id).await?;
    if let Some(client) = record.client_id {
        Access::new(&state, &current.user).client(client).await?;
    } else {
        current.require_admin()?;
    }
    let name = record.log_file.ok_or(AppError::NotFound)?;
    if name.contains('/') || name.starts_with('.') {
        return Err(AppError::NotFound);
    }
    let content = tokio::fs::read(state.engine.config.job_log_dir().join(&name))
        .await
        .map_err(|_| AppError::NotFound)?;
    Ok((
        [
            (header::CONTENT_TYPE, "text/plain; charset=utf-8".to_owned()),
            (header::CONTENT_DISPOSITION, format!("attachment; filename=\"{name}\"")),
        ],
        content,
    )
        .into_response())
}

struct QueueRow {
    client_id: i64,
    client: String,
    job_id: i64,
    job: String,
    state: String,
    enqueued: String,
    running_since: String,
    aborted: bool,
}

struct ClientStateRow {
    id: i64,
    name: String,
    state: String,
}

struct LocationUsage {
    name: String,
    directory: String,
    used: String,
    total: String,
    percent: u64,
}

#[derive(Template)]
#[template(path = "status.html")]
struct StatusPage {
    layout: Layout,
    queue: Vec<QueueRow>,
    clients: Vec<ClientStateRow>,
    locations: Vec<LocationUsage>,
}

async fn status(State(state): State<AppState>, current: CurrentUser) -> AppResult<Response> {
    let access = Access::new(&state, &current.user);
    let visible: Vec<ClientId> = access.clients(None).await?.iter().map(|c| c.id).collect();
    let store = state.store();
    let queue = store
        .queue()
        .list()
        .await?
        .into_iter()
        .filter(|e| visible.contains(&e.client_id))
        .map(|e| QueueRow {
            client_id: e.client_id.0,
            client: e.client_name,
            job_id: e.item.job_id.0,
            job: e.job_name,
            state: e.item.state.as_str().replace('_', " "),
            enqueued: format_time(e.item.enqueued_at),
            running_since: e.item.running_since.map(format_time).unwrap_or_default(),
            aborted: e.item.aborted,
        })
        .collect();
    let clients = access
        .clients(None)
        .await?
        .into_iter()
        .filter(|c| c.state != elkar_core::client::ClientState::Idle)
        .map(|c| ClientStateRow {
            id: c.id.0,
            state: c.state.as_str().replace('_', " "),
            name: c.spec.name,
        })
        .collect();
    let locations = if current.is_admin() {
        store
            .locations()
            .list()
            .await?
            .into_iter()
            .map(|l| {
                let dir = l.effective_dir();
                let usage = snapshots::fs_usage(&dir);
                LocationUsage {
                    name: l.spec.name,
                    directory: dir.display().to_string(),
                    used: usage
                        .map(|u| format_bytes(u.used_bytes))
                        .unwrap_or_else(|| "unavailable".into()),
                    total: usage.map(|u| format_bytes(u.total_bytes)).unwrap_or_default(),
                    percent: usage.map_or(0, |u| u.used_percent()),
                }
            })
            .collect()
    } else {
        Vec::new()
    };
    render(&StatusPage {
        layout: Layout::new(&state, &current, "status").await,
        queue,
        clients,
        locations,
    })
}

#[derive(Template)]
#[template(path = "about.html")]
struct AboutPage {
    layout: Layout,
    version: &'static str,
    public_key: Option<String>,
    key_path: String,
}

async fn about(State(state): State<AppState>, current: CurrentUser) -> AppResult<Response> {
    let key = &state.engine.config.ssh_key;
    render(&AboutPage {
        layout: Layout::new(&state, &current, "about").await,
        version: env!("CARGO_PKG_VERSION"),
        public_key: keys::read_public_key(key).await,
        key_path: keys::public_key_path(key).display().to_string(),
    })
}

/// The server's public key, unauthenticated so clients can fetch it while
/// being provisioned (`curl .../config/publickey >> authorized_keys`).
async fn public_key(State(state): State<AppState>) -> AppResult<Response> {
    let key = keys::read_public_key(&state.engine.config.ssh_key)
        .await
        .ok_or(AppError::NotFound)?;
    Ok((
        [
            (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
            (header::CONTENT_DISPOSITION, "attachment; filename=\"elkarbackup.pub\""),
        ],
        key,
    )
        .into_response())
}

async fn generate_key(State(state): State<AppState>, current: CurrentUser) -> AppResult<Response> {
    current.require_admin()?;
    keys::generate(state.engine.runner.as_ref(), &state.engine.config.ssh_key).await?;
    Access::new(&state, &current.user)
        .audit("SSH key pair regenerated", None)
        .await;
    current
        .flash(
            &state,
            "A new key pair was generated. Install the new public key on every client.",
        )
        .await;
    Ok(Redirect::to("/about").into_response())
}
