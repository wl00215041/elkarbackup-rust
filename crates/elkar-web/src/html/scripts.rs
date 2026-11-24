use askama::Template;
use axum::Router;
use axum::extract::{Multipart, Path, State};
use axum::http::header;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use elkar_core::ids::ScriptId;
use elkar_core::script::ScriptSpec;

use super::{FormErrors, Layout, format_time, render, split_validation};
use crate::AppState;
use crate::access::Access;
use crate::auth::CurrentUser;
use crate::error::{AppError, AppResult};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/scripts", get(list))
        .route("/script/new", get(new_form).post(create))
        .route("/script/{id}", get(edit_form).post(update))
        .route("/script/{id}/delete", post(delete))
        .route("/script/{id}/download", get(download))
}

struct ScriptRow {
    id: i64,
    name: String,
    description: String,
    usage: String,
    updated: String,
    file_exists: bool,
}

#[derive(Template)]
#[template(path = "scripts.html")]
struct ScriptsPage {
    layout: Layout,
    scripts: Vec<ScriptRow>,
}

fn usage(spec: &ScriptSpec) -> String {
    [
        (spec.is_client_pre, "client pre"),
        (spec.is_client_post, "client post"),
        (spec.is_job_pre, "job pre"),
        (spec.is_job_post, "job post"),
    ]
    .iter()
    .filter(|(on, _)| *on)
    .map(|(_, label)| *label)
    .collect::<Vec<_>>()
    .join(", ")
}

async fn list(State(state): State<AppState>, current: CurrentUser) -> AppResult<Response> {
    current.require_admin()?;
    let dir = &state.engine.config.scripts_dir;
    let scripts = state
        .store()
        .scripts()
        .list()
        .await?
        .into_iter()
        .map(|s| ScriptRow {
            id: s.id.0,
            usage: usage(&s.spec),
            updated: format_time(s.last_updated),
            file_exists: dir.join(s.file_name()).is_file(),
            name: s.spec.name,
            description: s.spec.description,
        })
        .collect();
    render(&ScriptsPage {
        layout: Layout::new(&state, &current, "scripts").await,
        scripts,
    })
}

#[derive(Template)]
#[template(path = "script_form.html")]
struct ScriptPage {
    layout: Layout,
    id: Option<i64>,
    form: ScriptSpec,
    errors: FormErrors,
}

async fn page(
    state: &AppState,
    current: &CurrentUser,
    id: Option<ScriptId>,
    form: ScriptSpec,
    errors: FormErrors,
) -> AppResult<Response> {
    render(&ScriptPage {
        layout: Layout::new(state, current, "scripts").await,
        id: id.map(|i| i.0),
        form,
        errors,
    })
}

async fn new_form(State(state): State<AppState>, current: CurrentUser) -> AppResult<Response> {
    current.require_admin()?;
    page(&state, &current, None, ScriptSpec::default(), FormErrors::default()).await
}

async fn edit_form(State(state): State<AppState>, current: CurrentUser, Path(id): Path<i64>) -> AppResult<Response> {
    current.require_admin()?;
    let script = state.store().scripts().get(ScriptId(id)).await?;
    page(&state, &current, Some(script.id), script.spec, FormErrors::default()).await
}

/// Reads the script form: text fields plus an optional uploaded `file`.
async fn read_form(mut multipart: Multipart) -> AppResult<(ScriptSpec, Option<Vec<u8>>)> {
    let mut spec = ScriptSpec::default();
    let mut content = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| AppError::BadRequest(e.to_string()))?
    {
        let name = field.name().unwrap_or_default().to_owned();
        if name == "file" {
            let bytes = field.bytes().await.map_err(|e| AppError::BadRequest(e.to_string()))?;
            content = Some(bytes.to_vec());
            continue;
        }
        let value = field.text().await.map_err(|e| AppError::BadRequest(e.to_string()))?;
        let checked = value == "true";
        match name.as_str() {
            "name" => spec.name = value,
            "description" => spec.description = value,
            "is_client_pre" => spec.is_client_pre = checked,
            "is_client_post" => spec.is_client_post = checked,
            "is_job_pre" => spec.is_job_pre = checked,
            "is_job_post" => spec.is_job_post = checked,
            _ => {}
        }
    }
    Ok((spec, content))
}

async fn save(
    state: AppState,
    current: CurrentUser,
    id: Option<ScriptId>,
    multipart: Multipart,
) -> AppResult<Response> {
    let (spec, content) = read_form(multipart).await?;
    let result = Access::new(&state, &current.user)
        .save_script(id, spec.clone(), content)
        .await;
    match split_validation(result)? {
        Ok(_) => {
            current.flash(&state, "Script saved").await;
            Ok(Redirect::to("/scripts").into_response())
        }
        Err(errors) => page(&state, &current, id, spec, errors).await,
    }
}

async fn create(State(state): State<AppState>, current: CurrentUser, multipart: Multipart) -> AppResult<Response> {
    save(state, current, None, multipart).await
}

async fn update(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(id): Path<i64>,
    multipart: Multipart,
) -> AppResult<Response> {
    save(state, current, Some(ScriptId(id)), multipart).await
}

async fn delete(State(state): State<AppState>, current: CurrentUser, Path(id): Path<i64>) -> AppResult<Response> {
    match Access::new(&state, &current.user).delete_script(ScriptId(id)).await {
        Ok(()) => current.flash(&state, "Script deleted").await,
        Err(AppError::Conflict(_)) => {
            current
                .flash(&state, "The script is in use and cannot be deleted")
                .await
        }
        Err(other) => return Err(other),
    }
    Ok(Redirect::to("/scripts").into_response())
}

async fn download(State(state): State<AppState>, current: CurrentUser, Path(id): Path<i64>) -> AppResult<Response> {
    current.require_admin()?;
    let script = state.store().scripts().get(ScriptId(id)).await?;
    let content = tokio::fs::read(state.engine.config.scripts_dir.join(script.file_name()))
        .await
        .map_err(|_| AppError::NotFound)?;
    Ok((
        [
            (header::CONTENT_TYPE, "text/x-shellscript".to_owned()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{}\"", script.file_name()),
            ),
        ],
        content,
    )
        .into_response())
}
