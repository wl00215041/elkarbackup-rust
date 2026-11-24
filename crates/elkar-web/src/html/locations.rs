use askama::Template;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use elkar_core::ValidationErrors;
use elkar_core::ids::LocationId;
use elkar_core::location::LocationSpec;
use serde::Deserialize;

use super::{FormErrors, Layout, field, render, split_validation};
use crate::AppState;
use crate::auth::CurrentUser;
use crate::error::AppResult;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/locations", get(list))
        .route("/location/new", get(new_form).post(create))
        .route("/location/{id}", get(edit_form).post(update))
        .route("/location/{id}/delete", post(delete))
}

struct LocationRow {
    id: i64,
    name: String,
    directory: String,
    max_parallel_jobs: u32,
}

#[derive(Template)]
#[template(path = "locations.html")]
struct LocationsPage {
    layout: Layout,
    locations: Vec<LocationRow>,
}

async fn list(State(state): State<AppState>, current: CurrentUser) -> AppResult<Response> {
    current.require_admin()?;
    let locations = state
        .store()
        .locations()
        .list()
        .await?
        .into_iter()
        .map(|l| LocationRow {
            id: l.id.0,
            directory: l.effective_dir().display().to_string(),
            max_parallel_jobs: l.spec.max_parallel_jobs,
            name: l.spec.name,
        })
        .collect();
    render(&LocationsPage {
        layout: Layout::new(&state, &current, "locations").await,
        locations,
    })
}

#[derive(Deserialize, Default)]
struct LocationForm {
    name: String,
    #[serde(default)]
    host: String,
    #[serde(default)]
    directory: String,
    #[serde(default)]
    max_parallel_jobs: String,
}

impl LocationForm {
    fn to_spec(&self) -> Result<LocationSpec, ValidationErrors> {
        let mut errors = ValidationErrors::new();
        let max_parallel_jobs = field::number(&mut errors, "max_parallel_jobs", &self.max_parallel_jobs, 1);
        errors.into_result()?;
        LocationSpec {
            name: self.name.clone(),
            host: Some(self.host.clone()),
            directory: self.directory.clone(),
            max_parallel_jobs,
        }
        .validate()
    }
}

#[derive(Template)]
#[template(path = "location_form.html")]
struct LocationPage {
    layout: Layout,
    id: Option<i64>,
    form: LocationForm,
    errors: FormErrors,
}

async fn page(
    state: &AppState,
    current: &CurrentUser,
    id: Option<LocationId>,
    form: LocationForm,
    errors: FormErrors,
) -> AppResult<Response> {
    render(&LocationPage {
        layout: Layout::new(state, current, "locations").await,
        id: id.map(|i| i.0),
        form,
        errors,
    })
}

async fn new_form(State(state): State<AppState>, current: CurrentUser) -> AppResult<Response> {
    current.require_admin()?;
    let form = LocationForm {
        max_parallel_jobs: "1".into(),
        ..LocationForm::default()
    };
    page(&state, &current, None, form, FormErrors::default()).await
}

async fn edit_form(State(state): State<AppState>, current: CurrentUser, Path(id): Path<i64>) -> AppResult<Response> {
    current.require_admin()?;
    let l = state.store().locations().get(LocationId(id)).await?;
    let form = LocationForm {
        name: l.spec.name,
        host: l.spec.host.unwrap_or_default(),
        directory: l.spec.directory,
        max_parallel_jobs: l.spec.max_parallel_jobs.to_string(),
    };
    page(&state, &current, Some(l.id), form, FormErrors::default()).await
}

async fn save(
    state: AppState,
    current: CurrentUser,
    id: Option<LocationId>,
    form: LocationForm,
) -> AppResult<Response> {
    current.require_admin()?;
    let store = state.store();
    let result = async {
        let spec = form.to_spec()?;
        match id {
            Some(id) => {
                store.locations().update(id, &spec).await?;
                Ok(id)
            }
            None => Ok(store.locations().create(&spec).await?),
        }
    }
    .await;
    match split_validation(result)? {
        Ok(_) => {
            current.flash(&state, "Backup location saved").await;
            Ok(Redirect::to("/locations").into_response())
        }
        Err(errors) => page(&state, &current, id, form, errors).await,
    }
}

async fn create(
    State(state): State<AppState>,
    current: CurrentUser,
    Form(form): Form<LocationForm>,
) -> AppResult<Response> {
    save(state, current, None, form).await
}

async fn update(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Form(form): Form<LocationForm>,
) -> AppResult<Response> {
    save(state, current, Some(LocationId(id)), form).await
}

async fn delete(State(state): State<AppState>, current: CurrentUser, Path(id): Path<i64>) -> AppResult<Response> {
    current.require_admin()?;
    match state.store().locations().delete(LocationId(id)).await {
        Ok(()) => current.flash(&state, "Backup location deleted").await,
        Err(elkar_storage::StorageError::InUse(_)) => {
            current
                .flash(&state, "The location is used by jobs and cannot be deleted")
                .await;
        }
        Err(other) => return Err(other.into()),
    }
    Ok(Redirect::to("/locations").into_response())
}
