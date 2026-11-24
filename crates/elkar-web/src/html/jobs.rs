use std::collections::HashMap;

use askama::Template;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::header;
use axum::response::{IntoResponse, Json, Redirect, Response};
use axum::routing::{get, post};
use axum_extra::extract::Form;
use elkar_core::ValidationErrors;
use elkar_core::ids::{ClientId, JobId, LocationId, PolicyId, ScriptId};
use elkar_core::job::{Job, JobSpec, Recipient};
use elkar_core::log::NotificationLevel;
use elkar_core::script::{Level, Phase};
use elkar_engine::EnqueueOutcome;
use elkar_engine::backup::BackupPlan;
use serde::Deserialize;
use serde_json::json;

use super::clients::{ScriptOption, script_options};
use super::{FormErrors, Layout, field, render, split_validation};
use crate::AppState;
use crate::access::Access;
use crate::auth::{CurrentUser, constant_time_eq};
use crate::error::{AppError, AppResult};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/client/{client}/job/new", get(new_form).post(create))
        .route("/client/{client}/job/{job}", get(edit_form).post(update))
        .route("/client/{client}/job/{job}/delete", post(delete))
        .route("/client/{client}/job/{job}/run", post(run))
        .route("/client/{client}/job/{job}/abort", post(abort))
        .route("/client/{client}/job/{job}/token", post(regenerate_token))
        .route("/client/{client}/job/{job}/token/delete", post(clear_token))
        .route("/client/{client}/job/{job}/config", get(show_config))
        .route("/jobs/sort", get(sort_form).post(sort))
}

#[derive(Deserialize, Default)]
struct JobForm {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    path: String,
    #[serde(default)]
    is_active: bool,
    #[serde(default)]
    policy_id: i64,
    #[serde(default)]
    location_id: i64,
    #[serde(default)]
    include: String,
    #[serde(default)]
    exclude: String,
    #[serde(default)]
    use_local_permissions: bool,
    #[serde(default)]
    notify_to: Vec<String>,
    #[serde(default)]
    notification_email: String,
    #[serde(default)]
    min_notification_level: String,
    #[serde(default)]
    priority: String,
    #[serde(default)]
    pre_scripts: Vec<i64>,
    #[serde(default)]
    post_scripts: Vec<i64>,
}

impl JobForm {
    fn from_job(job: &Job) -> Self {
        let s = &job.spec;
        Self {
            name: s.name.clone(),
            description: s.description.clone(),
            path: s.path.clone(),
            is_active: s.is_active,
            policy_id: s.policy_id.0,
            location_id: s.location_id.0,
            include: s.include.clone().unwrap_or_default(),
            exclude: s.exclude.clone().unwrap_or_default(),
            use_local_permissions: s.use_local_permissions,
            notify_to: s.notify_to.iter().map(|r| r.as_str().to_owned()).collect(),
            notification_email: s.notification_email.clone().unwrap_or_default(),
            min_notification_level: s.min_notification_level.as_str().to_owned(),
            priority: s.priority.to_string(),
            pre_scripts: s.pre_scripts.iter().map(|i| i.0).collect(),
            post_scripts: s.post_scripts.iter().map(|i| i.0).collect(),
        }
    }

    fn to_spec(&self) -> Result<JobSpec, ValidationErrors> {
        let mut errors = ValidationErrors::new();
        let priority = field::number(&mut errors, "priority", &self.priority, i32::MAX);
        let notify_to = self
            .notify_to
            .iter()
            .filter_map(|r| {
                r.parse::<Recipient>()
                    .map_err(|_| errors.add("notify_to", "unknown recipient"))
                    .ok()
            })
            .collect();
        let min_notification_level = self
            .min_notification_level
            .parse::<NotificationLevel>()
            .unwrap_or_else(|_| {
                errors.add("min_notification_level", "unknown level");
                NotificationLevel::Error
            });
        errors.into_result()?;
        Ok(JobSpec {
            name: self.name.clone(),
            description: self.description.clone(),
            path: self.path.clone(),
            is_active: self.is_active,
            policy_id: PolicyId(self.policy_id),
            location_id: LocationId(self.location_id),
            include: Some(self.include.clone()),
            exclude: Some(self.exclude.clone()),
            use_local_permissions: self.use_local_permissions,
            notify_to,
            notification_email: Some(self.notification_email.clone()),
            min_notification_level,
            priority,
            pre_scripts: self.pre_scripts.iter().copied().map(ScriptId).collect(),
            post_scripts: self.post_scripts.iter().copied().map(ScriptId).collect(),
        })
    }

    fn notifies(&self, recipient: &str) -> bool {
        self.notify_to.iter().any(|r| r == recipient)
    }
}

struct Choice {
    id: i64,
    name: String,
}

#[derive(Template)]
#[template(path = "job_form.html")]
struct JobPage {
    layout: Layout,
    client_id: i64,
    client_name: String,
    id: Option<i64>,
    form: JobForm,
    errors: FormErrors,
    policies: Vec<Choice>,
    locations: Vec<Choice>,
    pre_options: Vec<ScriptOption>,
    post_options: Vec<ScriptOption>,
    levels: Vec<&'static str>,
    token: Option<String>,
    run_url: String,
}

async fn page(
    state: &AppState,
    current: &CurrentUser,
    client: ClientId,
    id: Option<JobId>,
    form: JobForm,
    errors: FormErrors,
) -> AppResult<Response> {
    let access = Access::new(state, &current.user);
    let client = access.client(client).await?;
    let store = state.store();
    let scripts = store.scripts().list().await?;
    let token = match id {
        Some(id) => store.jobs().get(id).await?.token,
        None => None,
    };
    let policies = store
        .policies()
        .list()
        .await?
        .into_iter()
        .map(|p| Choice {
            id: p.id.0,
            name: p.spec.name,
        })
        .collect();
    let locations = store
        .locations()
        .list()
        .await?
        .into_iter()
        .map(|l| Choice {
            id: l.id.0,
            name: l.spec.name,
        })
        .collect();
    render(&JobPage {
        layout: Layout::new(state, current, "clients").await,
        client_id: client.id.0,
        client_name: client.spec.name,
        id: id.map(|i| i.0),
        pre_options: script_options(&scripts, Level::Job, Phase::Pre, &form.pre_scripts),
        post_options: script_options(&scripts, Level::Job, Phase::Post, &form.post_scripts),
        form,
        errors,
        policies,
        locations,
        levels: NotificationLevel::ALL.iter().map(|l| l.as_str()).collect(),
        token,
        run_url: format!(
            "{}/client/{}/job/{}/run",
            state.engine.config.public_url.trim_end_matches('/'),
            client.id,
            id.map(|i| i.0).unwrap_or_default()
        ),
    })
}

async fn new_form(State(state): State<AppState>, current: CurrentUser, Path(client): Path<i64>) -> AppResult<Response> {
    let store = state.store();
    let form = JobForm {
        is_active: true,
        use_local_permissions: true,
        notify_to: vec![Recipient::Owner.as_str().into()],
        min_notification_level: NotificationLevel::Error.as_str().into(),
        priority: "100".into(),
        policy_id: store.policies().list().await?.first().map_or(0, |p| p.id.0),
        location_id: store.locations().list().await?.first().map_or(0, |l| l.id.0),
        ..JobForm::default()
    };
    page(&state, &current, ClientId(client), None, form, FormErrors::default()).await
}

async fn edit_form(
    State(state): State<AppState>,
    current: CurrentUser,
    Path((client, job)): Path<(i64, i64)>,
) -> AppResult<Response> {
    let (_, job) = Access::new(&state, &current.user)
        .job(ClientId(client), JobId(job))
        .await?;
    let form = JobForm::from_job(&job);
    page(
        &state,
        &current,
        ClientId(client),
        Some(job.id),
        form,
        FormErrors::default(),
    )
    .await
}

async fn save(
    state: AppState,
    current: CurrentUser,
    client: ClientId,
    id: Option<JobId>,
    form: JobForm,
) -> AppResult<Response> {
    let result = match form.to_spec() {
        Ok(spec) => Access::new(&state, &current.user).save_job(client, id, spec).await,
        Err(errors) => Err(errors.into()),
    };
    match split_validation(result)? {
        Ok(saved) => {
            current.flash(&state, "Job saved").await;
            Ok(Redirect::to(&format!("/client/{client}/job/{saved}")).into_response())
        }
        Err(errors) => page(&state, &current, client, id, form, errors).await,
    }
}

async fn create(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(client): Path<i64>,
    Form(form): Form<JobForm>,
) -> AppResult<Response> {
    save(state, current, ClientId(client), None, form).await
}

async fn update(
    State(state): State<AppState>,
    current: CurrentUser,
    Path((client, job)): Path<(i64, i64)>,
    Form(form): Form<JobForm>,
) -> AppResult<Response> {
    save(state, current, ClientId(client), Some(JobId(job)), form).await
}

async fn delete(
    State(state): State<AppState>,
    current: CurrentUser,
    Path((client, job)): Path<(i64, i64)>,
) -> AppResult<Response> {
    Access::new(&state, &current.user)
        .delete_job(ClientId(client), JobId(job))
        .await?;
    current.flash(&state, "Job deleted").await;
    Ok(Redirect::to(&format!("/client/{client}")).into_response())
}

#[derive(Deserialize)]
struct TokenQuery {
    token: Option<String>,
}

/// Queues a job. Logged in users are redirected back; anonymous callers
/// must pass the job's `token` and receive JSON, for use from scripts.
async fn run(
    State(state): State<AppState>,
    current: Option<CurrentUser>,
    Path((client, job)): Path<(i64, i64)>,
    Query(query): Query<TokenQuery>,
) -> AppResult<Response> {
    let (client, job) = (ClientId(client), JobId(job));
    if let Some(current) = current {
        let outcome = Access::new(&state, &current.user).enqueue(client, job).await?;
        current
            .flash(
                &state,
                match outcome {
                    EnqueueOutcome::Queued => "Job queued. It will start shortly.",
                    EnqueueOutcome::AlreadyQueued => "The job was already queued.",
                },
            )
            .await;
        return Ok(Redirect::to("/clients").into_response());
    }

    let stored = state
        .store()
        .jobs()
        .get(job)
        .await
        .ok()
        .filter(|j| j.client_id == client);
    let authorized = match (&stored, &query.token) {
        (Some(job), Some(given)) => job.token.as_deref().is_some_and(|t| constant_time_eq(t, given)),
        _ => false,
    };
    if !authorized {
        return Ok((
            axum::http::StatusCode::FORBIDDEN,
            Json(json!({ "error": true, "msg": "You need to log in or send a valid token" })),
        )
            .into_response());
    }
    let outcome = state.engine.enqueue_now(client, job, Some("token")).await?;
    Ok(Json(match outcome {
        EnqueueOutcome::Queued => json!({ "error": false, "msg": "Job queued", "data": [job] }),
        EnqueueOutcome::AlreadyQueued => json!({ "error": true, "msg": "Job already queued", "data": [job] }),
    })
    .into_response())
}

async fn abort(
    State(state): State<AppState>,
    current: CurrentUser,
    Path((client, job)): Path<(i64, i64)>,
) -> AppResult<Response> {
    let aborted = Access::new(&state, &current.user)
        .abort(ClientId(client), JobId(job))
        .await?;
    current
        .flash(
            &state,
            if aborted {
                "Abort requested; the job stops shortly."
            } else {
                "The job is not queued; it has probably finished."
            },
        )
        .await;
    Ok(Redirect::to("/clients").into_response())
}

async fn regenerate_token(
    State(state): State<AppState>,
    current: CurrentUser,
    Path((client, job)): Path<(i64, i64)>,
) -> AppResult<Response> {
    Access::new(&state, &current.user)
        .regenerate_token(ClientId(client), JobId(job))
        .await?;
    current.flash(&state, "New trigger token generated").await;
    Ok(Redirect::to(&format!("/client/{client}/job/{job}")).into_response())
}

async fn clear_token(
    State(state): State<AppState>,
    current: CurrentUser,
    Path((client, job)): Path<(i64, i64)>,
) -> AppResult<Response> {
    Access::new(&state, &current.user)
        .clear_token(ClientId(client), JobId(job))
        .await?;
    current.flash(&state, "Trigger token removed").await;
    Ok(Redirect::to(&format!("/client/{client}/job/{job}")).into_response())
}

/// The rsnapshot configuration the job runs with, as plain text.
async fn show_config(
    State(state): State<AppState>,
    current: CurrentUser,
    Path((client, job)): Path<(i64, i64)>,
) -> AppResult<Response> {
    let (client, job) = Access::new(&state, &current.user)
        .job(ClientId(client), JobId(job))
        .await?;
    let store = state.store();
    let policy = store.policies().get(job.spec.policy_id).await?;
    let location = store.locations().get(job.spec.location_id).await?;
    let plan = BackupPlan {
        job: &job,
        client: &client,
        policy: &policy,
        location: &location,
        levels: Vec::new(),
    };
    let text = plan.rsnapshot_config(&state.engine.config).render();
    Ok(([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], text).into_response())
}

struct SortRow {
    id: i64,
    client: String,
    name: String,
    priority: i32,
}

#[derive(Template)]
#[template(path = "sort_jobs.html")]
struct SortPage {
    layout: Layout,
    jobs: Vec<SortRow>,
}

async fn sort_form(State(state): State<AppState>, current: CurrentUser) -> AppResult<Response> {
    current.require_admin()?;
    let store = state.store();
    let clients: HashMap<_, _> = store
        .clients()
        .list(&Default::default())
        .await?
        .into_iter()
        .map(|c| (c.id, c.spec.name))
        .collect();
    let mut jobs: Vec<SortRow> = store
        .jobs()
        .list()
        .await?
        .into_iter()
        .map(|j| SortRow {
            id: j.id.0,
            client: clients.get(&j.client_id).cloned().unwrap_or_default(),
            name: j.spec.name,
            priority: j.spec.priority,
        })
        .collect();
    jobs.sort_by_key(|j| (j.priority, j.id));
    render(&SortPage {
        layout: Layout::new(&state, &current, "sort").await,
        jobs,
    })
}

/// Accepts `priority_<job id>=<number>` fields and renumbers jobs by them.
async fn sort(
    State(state): State<AppState>,
    current: CurrentUser,
    Form(fields): Form<Vec<(String, String)>>,
) -> AppResult<Response> {
    current.require_admin()?;
    let mut order: Vec<(i64, JobId)> = fields
        .iter()
        .filter_map(|(key, value)| {
            let id = key.strip_prefix("priority_")?.parse().ok()?;
            Some((value.trim().parse().ok()?, JobId(id)))
        })
        .collect();
    if order.is_empty() {
        return Err(AppError::BadRequest("no priorities submitted".into()));
    }
    order.sort();
    let ordered: Vec<JobId> = order.into_iter().map(|(_, id)| id).collect();
    state.store().jobs().set_priorities(&ordered).await?;
    current.flash(&state, "Job priorities saved").await;
    Ok(Redirect::to("/jobs/sort").into_response())
}
