//! Browsing, downloading and restoring snapshot contents.

use std::path::PathBuf;
use std::process::Stdio;

use askama::Template;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::header;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::{Form, Router};
use elkar_core::client::Client;
use elkar_core::ids::{ClientId, JobId};
use elkar_core::job::Job;
use elkar_core::log::Subject;
use elkar_engine::restore::RestoreRequest;
use elkar_engine::snapshots;
use serde::Deserialize;
use tokio_util::io::ReaderStream;

use super::{Layout, format_bytes, render};
use crate::AppState;
use crate::access::Access;
use crate::auth::CurrentUser;
use crate::error::{AppError, AppResult};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/client/{client}/job/{job}/backup/", get(browse_root))
        .route("/client/{client}/job/{job}/backup/{*path}", get(browse))
        .route(
            "/client/{client}/job/{job}/restore/{*path}",
            get(restore_form).post(restore),
        )
}

/// Resolves a path inside the job's snapshot root after an access check.
async fn locate(
    state: &AppState,
    current: &CurrentUser,
    client: i64,
    job: i64,
    path: &str,
) -> AppResult<(Client, Job, PathBuf, PathBuf)> {
    let (client, job) = Access::new(state, &current.user)
        .job(ClientId(client), JobId(job))
        .await?;
    let location = state.store().locations().get(job.spec.location_id).await?;
    let root = location.snapshot_root(client.id, job.id);
    if !root.is_dir() {
        return Err(AppError::NotFound);
    }
    let resolved = snapshots::resolve(&root, path).await?;
    let root = tokio::fs::canonicalize(&root).await.map_err(AppError::internal)?;
    Ok((client, job, root, resolved))
}

struct Crumb {
    name: String,
    path: String,
}

struct EntryRow {
    name: String,
    path: String,
    is_dir: bool,
    is_symlink: bool,
    size: String,
    modified: String,
}

#[derive(Template)]
#[template(path = "browse.html")]
struct BrowsePage {
    layout: Layout,
    client_id: i64,
    client_name: String,
    job_id: i64,
    job_name: String,
    path: String,
    crumbs: Vec<Crumb>,
    entries: Vec<EntryRow>,
}

#[derive(Deserialize)]
struct BrowseQuery {
    #[serde(default)]
    download: bool,
}

async fn browse_root(
    state: State<AppState>,
    current: CurrentUser,
    Path((client, job)): Path<(i64, i64)>,
    query: Query<BrowseQuery>,
) -> AppResult<Response> {
    browse(state, current, Path((client, job, String::new())), query).await
}

async fn browse(
    State(state): State<AppState>,
    current: CurrentUser,
    Path((client, job, path)): Path<(i64, i64, String)>,
    Query(query): Query<BrowseQuery>,
) -> AppResult<Response> {
    let (client, job, root, resolved) = locate(&state, &current, client, job, &path).await?;
    let relative = resolved
        .strip_prefix(&root)
        .map_err(|_| AppError::NotFound)?
        .to_string_lossy()
        .into_owned();

    if query.download || !resolved.is_dir() {
        Access::new(&state, &current.user)
            .audit(format!("Downloaded {relative}"), Some(Subject::Job(client.id, job.id)))
            .await;
        return if resolved.is_dir() {
            download_directory(&resolved).await
        } else {
            download_file(&resolved).await
        };
    }

    let entries = snapshots::list_dir(&resolved)
        .await?
        .into_iter()
        .map(|e| EntryRow {
            path: join(&relative, &e.name),
            size: if e.is_dir { String::new() } else { format_bytes(e.size) },
            modified: e.modified.map(super::format_time).unwrap_or_default(),
            name: e.name,
            is_dir: e.is_dir,
            is_symlink: e.is_symlink,
        })
        .collect();
    let mut crumbs = Vec::new();
    let mut acc = String::new();
    for part in relative.split('/').filter(|p| !p.is_empty()) {
        acc = join(&acc, part);
        crumbs.push(Crumb {
            name: part.to_owned(),
            path: acc.clone(),
        });
    }
    render(&BrowsePage {
        layout: Layout::new(&state, &current, "clients").await,
        client_id: client.id.0,
        client_name: client.spec.name,
        job_id: job.id.0,
        job_name: job.spec.name,
        path: relative,
        crumbs,
        entries,
    })
}

fn join(base: &str, name: &str) -> String {
    if base.is_empty() {
        name.to_owned()
    } else {
        format!("{base}/{name}")
    }
}

fn attachment(name: &str) -> String {
    let safe: String = name
        .chars()
        .map(|c| if c == '"' || c.is_control() { '_' } else { c })
        .collect();
    format!("attachment; filename=\"{safe}\"")
}

async fn download_file(path: &std::path::Path) -> AppResult<Response> {
    let file = tokio::fs::File::open(path).await.map_err(|_| AppError::NotFound)?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    Ok((
        [
            (header::CONTENT_TYPE, "application/octet-stream".to_owned()),
            (header::CONTENT_DISPOSITION, attachment(&name)),
        ],
        Body::from_stream(ReaderStream::new(file)),
    )
        .into_response())
}

/// Streams a directory as `.tar.gz` straight from `tar`'s stdout.
async fn download_directory(path: &std::path::Path) -> AppResult<Response> {
    let parent = path.parent().ok_or(AppError::NotFound)?;
    let name = path.file_name().ok_or(AppError::NotFound)?;
    let mut child = tokio::process::Command::new("tar")
        .arg("-C")
        .arg(parent)
        .arg("-czf")
        .arg("-")
        .arg("--")
        .arg(name)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(AppError::internal)?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| AppError::internal("tar has no stdout"))?;
    tokio::spawn(async move {
        let _ = child.wait().await;
    });
    Ok((
        [
            (header::CONTENT_TYPE, "application/gzip".to_owned()),
            (
                header::CONTENT_DISPOSITION,
                attachment(&format!("{}.tar.gz", name.to_string_lossy())),
            ),
        ],
        Body::from_stream(ReaderStream::new(stdout)),
    )
        .into_response())
}

struct TargetOption {
    id: i64,
    name: String,
}

#[derive(Template)]
#[template(path = "restore.html")]
struct RestorePage {
    layout: Layout,
    client_id: i64,
    job_id: i64,
    path: String,
    targets: Vec<TargetOption>,
    target_path: String,
}

/// Snapshot paths look like `daily.0/etc/hosts`; the original location of
/// that item's parent is `/etc`.
fn suggested_target(path: &str) -> String {
    let without_level = path.split_once('/').map_or("", |(_, rest)| rest);
    match without_level.rsplit_once('/') {
        Some((parent, _)) => format!("/{parent}"),
        None => "/".to_owned(),
    }
}

async fn restore_form(
    State(state): State<AppState>,
    current: CurrentUser,
    Path((client, job, path)): Path<(i64, i64, String)>,
) -> AppResult<Response> {
    let (client, job, root, resolved) = locate(&state, &current, client, job, &path).await?;
    let relative = resolved
        .strip_prefix(&root)
        .map_err(|_| AppError::NotFound)?
        .to_string_lossy()
        .into_owned();
    let mut targets: Vec<TargetOption> = Access::new(&state, &current.user)
        .clients(None)
        .await?
        .into_iter()
        .map(|c| TargetOption {
            id: c.id.0,
            name: c.spec.name,
        })
        .collect();
    // The original client first, as the default choice.
    targets.sort_by_key(|t| t.id != client.id.0);
    render(&RestorePage {
        layout: Layout::new(&state, &current, "clients").await,
        client_id: client.id.0,
        job_id: job.id.0,
        target_path: suggested_target(&relative),
        path: relative,
        targets,
    })
}

#[derive(Deserialize)]
struct RestoreForm {
    target_client: i64,
    target_path: String,
}

async fn restore(
    State(state): State<AppState>,
    current: CurrentUser,
    Path((client, job, path)): Path<(i64, i64, String)>,
    Form(form): Form<RestoreForm>,
) -> AppResult<Response> {
    let (_, _, _, source) = locate(&state, &current, client, job, &path).await?;
    let target = Access::new(&state, &current.user)
        .client(ClientId(form.target_client))
        .await?;
    let target_path = form.target_path.trim().to_owned();
    if !target_path.starts_with('/') || target_path.contains(['\n', '\r']) {
        return Err(AppError::BadRequest("the target path must be absolute".into()));
    }
    state.engine.spawn_restore(RestoreRequest {
        source,
        target,
        target_path,
        requested_by: current.name().to_owned(),
    });
    current
        .flash(&state, "Restore started; its progress is reported in the logs.")
        .await;
    Ok(Redirect::to(&format!("/logs?client={}", form.target_client)).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggests_parent_of_original_location() {
        assert_eq!(suggested_target("daily.0/etc/hosts"), "/etc");
        assert_eq!(suggested_target("daily.0/etc"), "/");
        assert_eq!(suggested_target("daily.0"), "/");
    }
}
