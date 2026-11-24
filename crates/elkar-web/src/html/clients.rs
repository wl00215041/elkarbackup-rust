use std::collections::HashMap;

use askama::Template;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum_extra::extract::Form;
use elkar_core::ValidationErrors;
use elkar_core::client::{Client, ClientSpec};
use elkar_core::ids::{ClientId, ScriptId, UserId};
use elkar_core::job::BackupResult;
use elkar_core::script::{Level, Phase, Script};
use elkar_engine::STATUS_SOURCE;
use serde::Deserialize;

use super::{FormErrors, Layout, field, format_kb, render, split_validation};
use crate::AppState;
use crate::access::Access;
use crate::auth::CurrentUser;
use crate::error::AppResult;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/clients", get(list))
        .route("/client/new", get(new_form).post(create))
        .route("/client/{id}", get(edit_form).post(update))
        .route("/client/{id}/delete", post(delete))
        .route("/client/{id}/clone", post(clone))
}

struct ClientRow {
    id: i64,
    name: String,
    url: String,
    description: String,
    state: String,
    is_active: bool,
    disk: String,
    quota: String,
    over_quota: bool,
    jobs: Vec<JobRow>,
}

struct JobRow {
    id: i64,
    name: String,
    path: String,
    policy: String,
    result: &'static str,
    status: String,
    queue: String,
    disk: String,
    is_active: bool,
}

#[derive(Template)]
#[template(path = "clients.html")]
struct ClientsPage {
    layout: Layout,
    filter: String,
    clients: Vec<ClientRow>,
}

#[derive(Deserialize)]
struct ListQuery {
    #[serde(default)]
    name: String,
}

async fn list(
    State(state): State<AppState>,
    current: CurrentUser,
    Query(query): Query<ListQuery>,
) -> AppResult<Response> {
    let access = Access::new(&state, &current.user);
    let store = state.store();
    let clients = access.clients(Some(query.name.clone())).await?;
    let policies: HashMap<_, _> = store
        .policies()
        .list()
        .await?
        .into_iter()
        .map(|p| (p.id, p.spec.name))
        .collect();
    let statuses: HashMap<_, _> = store
        .logs()
        .latest_status_per_job(STATUS_SOURCE)
        .await?
        .into_iter()
        .filter_map(|r| r.job_id.map(|j| (j, r.message)))
        .collect();
    let queue: HashMap<_, _> = store
        .queue()
        .list()
        .await?
        .into_iter()
        .map(|e| (e.item.job_id, e.item.state.as_str().replace('_', " ")))
        .collect();
    let mut jobs_by_client: HashMap<ClientId, Vec<JobRow>> = HashMap::new();
    let mut usage: HashMap<ClientId, i64> = HashMap::new();
    for job in access.jobs().await? {
        *usage.entry(job.client_id).or_default() += job.disk_usage_kb;
        jobs_by_client.entry(job.client_id).or_default().push(JobRow {
            id: job.id.0,
            name: job.spec.name,
            path: job.spec.path,
            policy: policies.get(&job.spec.policy_id).cloned().unwrap_or_default(),
            result: match job.last_result {
                Some(BackupResult::Ok) => "ok",
                Some(BackupResult::Failed) => "failed",
                None => "",
            },
            status: statuses.get(&job.id).cloned().unwrap_or_default(),
            queue: queue.get(&job.id).cloned().unwrap_or_default(),
            disk: format_kb(job.disk_usage_kb),
            is_active: job.spec.is_active,
        });
    }
    let clients = clients
        .into_iter()
        .map(|c| {
            let used = usage.get(&c.id).copied().unwrap_or(0);
            ClientRow {
                id: c.id.0,
                url: if c.spec.url.is_empty() {
                    "(local)".into()
                } else {
                    c.spec.url.clone()
                },
                description: c.spec.description,
                state: c.state.as_str().replace('_', " "),
                is_active: c.spec.is_active,
                disk: format_kb(used),
                quota: c.spec.quota_kb.map_or_else(|| "unlimited".into(), format_kb),
                over_quota: c.spec.quota_kb.is_some_and(|q| used > q),
                jobs: jobs_by_client.remove(&c.id).unwrap_or_default(),
                name: c.spec.name,
            }
        })
        .collect();
    render(&ClientsPage {
        layout: Layout::new(&state, &current, "clients").await,
        filter: query.name,
        clients,
    })
}

#[derive(Deserialize, Default)]
pub struct ClientForm {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    is_active: bool,
    #[serde(default)]
    quota_kb: String,
    #[serde(default)]
    owner_id: String,
    #[serde(default)]
    ssh_args: String,
    #[serde(default)]
    rsync_short_args: String,
    #[serde(default)]
    rsync_long_args: String,
    #[serde(default)]
    max_parallel_jobs: String,
    #[serde(default)]
    pre_scripts: Vec<i64>,
    #[serde(default)]
    post_scripts: Vec<i64>,
}

impl ClientForm {
    fn from_client(client: &Client) -> Self {
        let s = &client.spec;
        Self {
            name: s.name.clone(),
            description: s.description.clone(),
            url: s.url.clone(),
            is_active: s.is_active,
            quota_kb: s.quota_kb.map(|q| q.to_string()).unwrap_or_default(),
            owner_id: s.owner_id.map(|o| o.to_string()).unwrap_or_default(),
            ssh_args: s.ssh_args.clone(),
            rsync_short_args: s.rsync_short_args.clone(),
            rsync_long_args: s.rsync_long_args.clone(),
            max_parallel_jobs: s.max_parallel_jobs.to_string(),
            pre_scripts: s.pre_scripts.iter().map(|i| i.0).collect(),
            post_scripts: s.post_scripts.iter().map(|i| i.0).collect(),
        }
    }

    fn to_spec(&self) -> Result<ClientSpec, ValidationErrors> {
        let mut errors = ValidationErrors::new();
        let quota_kb = field::optional_i64(&mut errors, "quota_kb", &self.quota_kb);
        let owner_id = field::optional_i64(&mut errors, "owner_id", &self.owner_id).map(UserId);
        let max_parallel_jobs = field::number(&mut errors, "max_parallel_jobs", &self.max_parallel_jobs, 1);
        errors.into_result()?;
        Ok(ClientSpec {
            name: self.name.clone(),
            description: self.description.clone(),
            url: self.url.clone(),
            is_active: self.is_active,
            quota_kb,
            owner_id,
            ssh_args: self.ssh_args.clone(),
            rsync_short_args: self.rsync_short_args.clone(),
            rsync_long_args: self.rsync_long_args.clone(),
            max_parallel_jobs,
            pre_scripts: self.pre_scripts.iter().copied().map(ScriptId).collect(),
            post_scripts: self.post_scripts.iter().copied().map(ScriptId).collect(),
        })
    }
}

/// An entry of a script multi-select.
pub struct ScriptOption {
    pub id: i64,
    pub name: String,
    pub selected: bool,
}

pub fn script_options(scripts: &[Script], level: Level, phase: Phase, selected: &[i64]) -> Vec<ScriptOption> {
    scripts
        .iter()
        .filter(|s| s.spec.allows(level, phase))
        .map(|s| ScriptOption {
            id: s.id.0,
            name: s.spec.name.clone(),
            selected: selected.contains(&s.id.0),
        })
        .collect()
}

struct UserOption {
    id: String,
    name: String,
}

struct ClientJob {
    id: i64,
    name: String,
    path: String,
}

#[derive(Template)]
#[template(path = "client_form.html")]
struct ClientPage {
    layout: Layout,
    id: Option<i64>,
    form: ClientForm,
    errors: FormErrors,
    users: Vec<UserOption>,
    pre_options: Vec<ScriptOption>,
    post_options: Vec<ScriptOption>,
    jobs: Vec<ClientJob>,
}

async fn page(
    state: &AppState,
    current: &CurrentUser,
    id: Option<ClientId>,
    form: ClientForm,
    errors: FormErrors,
) -> AppResult<Response> {
    let store = state.store();
    let scripts = store.scripts().list().await?;
    let users = if current.is_admin() {
        store
            .users()
            .list()
            .await?
            .into_iter()
            .map(|u| UserOption {
                id: u.id.to_string(),
                name: u.spec.username,
            })
            .collect()
    } else {
        Vec::new()
    };
    let jobs = match id {
        Some(id) => store
            .jobs()
            .list_for_client(id)
            .await?
            .into_iter()
            .map(|j| ClientJob {
                id: j.id.0,
                name: j.spec.name,
                path: j.spec.path,
            })
            .collect(),
        None => Vec::new(),
    };
    render(&ClientPage {
        layout: Layout::new(state, current, "clients").await,
        id: id.map(|i| i.0),
        pre_options: script_options(&scripts, Level::Client, Phase::Pre, &form.pre_scripts),
        post_options: script_options(&scripts, Level::Client, Phase::Post, &form.post_scripts),
        form,
        errors,
        users,
        jobs,
    })
}

async fn new_form(State(state): State<AppState>, current: CurrentUser) -> AppResult<Response> {
    let form = ClientForm {
        is_active: true,
        max_parallel_jobs: "1".into(),
        owner_id: current.user.id.to_string(),
        ..ClientForm::default()
    };
    page(&state, &current, None, form, FormErrors::default()).await
}

async fn edit_form(State(state): State<AppState>, current: CurrentUser, Path(id): Path<i64>) -> AppResult<Response> {
    let client = Access::new(&state, &current.user).client(ClientId(id)).await?;
    page(
        &state,
        &current,
        Some(client.id),
        ClientForm::from_client(&client),
        FormErrors::default(),
    )
    .await
}

async fn save(state: AppState, current: CurrentUser, id: Option<ClientId>, form: ClientForm) -> AppResult<Response> {
    let result = match form.to_spec() {
        Ok(spec) => Access::new(&state, &current.user).save_client(id, spec).await,
        Err(errors) => Err(errors.into()),
    };
    match split_validation(result)? {
        Ok(saved) => {
            current.flash(&state, "Client saved").await;
            Ok(Redirect::to(&format!("/client/{saved}")).into_response())
        }
        Err(errors) => page(&state, &current, id, form, errors).await,
    }
}

async fn create(
    State(state): State<AppState>,
    current: CurrentUser,
    Form(form): Form<ClientForm>,
) -> AppResult<Response> {
    save(state, current, None, form).await
}

async fn update(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Form(form): Form<ClientForm>,
) -> AppResult<Response> {
    save(state, current, Some(ClientId(id)), form).await
}

async fn delete(State(state): State<AppState>, current: CurrentUser, Path(id): Path<i64>) -> AppResult<Response> {
    Access::new(&state, &current.user).delete_client(ClientId(id)).await?;
    current.flash(&state, "Client deleted").await;
    Ok(Redirect::to("/clients").into_response())
}

async fn clone(State(state): State<AppState>, current: CurrentUser, Path(id): Path<i64>) -> AppResult<Response> {
    let copy = Access::new(&state, &current.user).clone_client(ClientId(id)).await?;
    current.flash(&state, "Client cloned").await;
    Ok(Redirect::to(&format!("/client/{copy}")).into_response())
}
