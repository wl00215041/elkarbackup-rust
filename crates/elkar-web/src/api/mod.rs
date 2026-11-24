//! JSON REST API, authenticated with HTTP Basic.

use axum::extract::{FromRequestParts, Path, Query, State};
use axum::http::request::Parts;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use elkar_core::client::{Client, ClientSpec};
use elkar_core::ids::{ClientId, JobId, LocationId, PolicyId};
use elkar_core::job::{Job, JobSpec};
use elkar_core::location::{BackupLocation, LocationSpec};
use elkar_core::log::LogRecord;
use elkar_core::policy::{Policy, PolicySpec};
use elkar_core::script::Script;
use elkar_core::user::User;
use elkar_engine::EnqueueOutcome;
use elkar_storage::LogFilter;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::AppState;
use crate::access::Access;
use crate::auth::{authenticate, basic_credentials};
use crate::error::AppError;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/clients", get(list_clients).post(create_client))
        .route(
            "/clients/{id}",
            get(get_client).put(update_client).delete(delete_client),
        )
        .route("/jobs", get(list_jobs).post(create_job))
        .route("/jobs/{id}", get(get_job).put(update_job).delete(delete_job))
        .route("/jobs/{id}/run", post(run_job))
        .route("/jobs/{id}/abort", post(abort_job))
        .route("/policies", get(list_policies).post(create_policy))
        .route(
            "/policies/{id}",
            get(get_policy).put(update_policy).delete(delete_policy),
        )
        .route("/backup_locations", get(list_locations).post(create_location))
        .route(
            "/backup_locations/{id}",
            get(get_location).put(update_location).delete(delete_location),
        )
        .route("/scripts", get(list_scripts))
        .route("/users", get(list_users))
        .route("/queue", get(queue))
        .route("/logs", get(logs))
}

/// [`AppError`] rendered as JSON.
pub struct ApiError(AppError);

impl<E: Into<AppError>> From<E> for ApiError {
    fn from(err: E) -> Self {
        Self(err.into())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = self.0.status();
        if let AppError::Internal(detail) = &self.0 {
            tracing::error!(%detail, "api request failed");
        }
        let mut body = json!({ "error": self.0.to_string() });
        if let AppError::Invalid(errors) = &self.0 {
            body["details"] = json!(errors);
        }
        let mut response = (status, Json(body)).into_response();
        if status == StatusCode::UNAUTHORIZED {
            response.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                header::HeaderValue::from_static("Basic realm=\"ElkarBackup API\""),
            );
        }
        response
    }
}

type ApiResult<T> = Result<Json<T>, ApiError>;

/// The user authenticated by the request's Basic credentials.
pub struct ApiUser(User);

impl FromRequestParts<AppState> for ApiUser {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        let (username, password) = basic_credentials(parts).ok_or(AppError::Unauthorized)?;
        let user = authenticate(state, &username, &password)
            .await?
            .ok_or(AppError::Unauthorized)?;
        Ok(Self(user))
    }
}

#[derive(Serialize)]
struct Created {
    id: i64,
}

fn created(id: i64) -> Response {
    (StatusCode::CREATED, Json(Created { id })).into_response()
}

// ----- clients -----------------------------------------------------------

async fn list_clients(State(state): State<AppState>, ApiUser(user): ApiUser) -> ApiResult<Vec<Client>> {
    Ok(Json(Access::new(&state, &user).clients(None).await?))
}

async fn get_client(State(state): State<AppState>, ApiUser(user): ApiUser, Path(id): Path<i64>) -> ApiResult<Client> {
    Ok(Json(Access::new(&state, &user).client(ClientId(id)).await?))
}

async fn create_client(
    State(state): State<AppState>,
    ApiUser(user): ApiUser,
    Json(spec): Json<ClientSpec>,
) -> Result<Response, ApiError> {
    let id = Access::new(&state, &user).save_client(None, spec).await?;
    Ok(created(id.0))
}

async fn update_client(
    State(state): State<AppState>,
    ApiUser(user): ApiUser,
    Path(id): Path<i64>,
    Json(spec): Json<ClientSpec>,
) -> ApiResult<Client> {
    let access = Access::new(&state, &user);
    let id = access.save_client(Some(ClientId(id)), spec).await?;
    Ok(Json(access.client(id).await?))
}

async fn delete_client(
    State(state): State<AppState>,
    ApiUser(user): ApiUser,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiError> {
    Access::new(&state, &user).delete_client(ClientId(id)).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ----- jobs --------------------------------------------------------------

#[derive(Deserialize)]
struct NewJob {
    client_id: ClientId,
    #[serde(flatten)]
    spec: JobSpec,
}

async fn list_jobs(State(state): State<AppState>, ApiUser(user): ApiUser) -> ApiResult<Vec<Job>> {
    Ok(Json(Access::new(&state, &user).jobs().await?))
}

async fn get_job(State(state): State<AppState>, ApiUser(user): ApiUser, Path(id): Path<i64>) -> ApiResult<Job> {
    let (_, job) = Access::new(&state, &user).job_by_id(JobId(id)).await?;
    Ok(Json(job))
}

async fn create_job(
    State(state): State<AppState>,
    ApiUser(user): ApiUser,
    Json(new): Json<NewJob>,
) -> Result<Response, ApiError> {
    let id = Access::new(&state, &user)
        .save_job(new.client_id, None, new.spec)
        .await?;
    Ok(created(id.0))
}

async fn update_job(
    State(state): State<AppState>,
    ApiUser(user): ApiUser,
    Path(id): Path<i64>,
    Json(spec): Json<JobSpec>,
) -> ApiResult<Job> {
    let access = Access::new(&state, &user);
    let (client, _) = access.job_by_id(JobId(id)).await?;
    access.save_job(client.id, Some(JobId(id)), spec).await?;
    let (_, job) = access.job_by_id(JobId(id)).await?;
    Ok(Json(job))
}

async fn delete_job(
    State(state): State<AppState>,
    ApiUser(user): ApiUser,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiError> {
    let access = Access::new(&state, &user);
    let (client, _) = access.job_by_id(JobId(id)).await?;
    access.delete_job(client.id, JobId(id)).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn run_job(
    State(state): State<AppState>,
    ApiUser(user): ApiUser,
    Path(id): Path<i64>,
) -> ApiResult<serde_json::Value> {
    let access = Access::new(&state, &user);
    let (client, _) = access.job_by_id(JobId(id)).await?;
    let queued = access.enqueue(client.id, JobId(id)).await? == EnqueueOutcome::Queued;
    Ok(Json(json!({ "queued": queued })))
}

async fn abort_job(
    State(state): State<AppState>,
    ApiUser(user): ApiUser,
    Path(id): Path<i64>,
) -> ApiResult<serde_json::Value> {
    let access = Access::new(&state, &user);
    let (client, _) = access.job_by_id(JobId(id)).await?;
    let aborting = access.abort(client.id, JobId(id)).await?;
    Ok(Json(json!({ "aborting": aborting })))
}

// ----- policies ----------------------------------------------------------

async fn list_policies(State(state): State<AppState>, ApiUser(_): ApiUser) -> ApiResult<Vec<Policy>> {
    Ok(Json(state.store().policies().list().await?))
}

async fn get_policy(State(state): State<AppState>, ApiUser(_): ApiUser, Path(id): Path<i64>) -> ApiResult<Policy> {
    Ok(Json(state.store().policies().get(PolicyId(id)).await?))
}

async fn create_policy(
    State(state): State<AppState>,
    ApiUser(user): ApiUser,
    Json(spec): Json<PolicySpec>,
) -> Result<Response, ApiError> {
    Access::new(&state, &user).require_admin()?;
    let id = state.store().policies().create(&spec.validate()?).await?;
    Ok(created(id.0))
}

async fn update_policy(
    State(state): State<AppState>,
    ApiUser(user): ApiUser,
    Path(id): Path<i64>,
    Json(spec): Json<PolicySpec>,
) -> ApiResult<Policy> {
    Access::new(&state, &user).require_admin()?;
    state.store().policies().update(PolicyId(id), &spec.validate()?).await?;
    Ok(Json(state.store().policies().get(PolicyId(id)).await?))
}

async fn delete_policy(
    State(state): State<AppState>,
    ApiUser(user): ApiUser,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiError> {
    Access::new(&state, &user).require_admin()?;
    state.store().policies().delete(PolicyId(id)).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ----- backup locations --------------------------------------------------

async fn list_locations(State(state): State<AppState>, ApiUser(_): ApiUser) -> ApiResult<Vec<BackupLocation>> {
    Ok(Json(state.store().locations().list().await?))
}

async fn get_location(
    State(state): State<AppState>,
    ApiUser(_): ApiUser,
    Path(id): Path<i64>,
) -> ApiResult<BackupLocation> {
    Ok(Json(state.store().locations().get(LocationId(id)).await?))
}

async fn create_location(
    State(state): State<AppState>,
    ApiUser(user): ApiUser,
    Json(spec): Json<LocationSpec>,
) -> Result<Response, ApiError> {
    Access::new(&state, &user).require_admin()?;
    let id = state.store().locations().create(&spec.validate()?).await?;
    Ok(created(id.0))
}

async fn update_location(
    State(state): State<AppState>,
    ApiUser(user): ApiUser,
    Path(id): Path<i64>,
    Json(spec): Json<LocationSpec>,
) -> ApiResult<BackupLocation> {
    Access::new(&state, &user).require_admin()?;
    state
        .store()
        .locations()
        .update(LocationId(id), &spec.validate()?)
        .await?;
    Ok(Json(state.store().locations().get(LocationId(id)).await?))
}

async fn delete_location(
    State(state): State<AppState>,
    ApiUser(user): ApiUser,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiError> {
    Access::new(&state, &user).require_admin()?;
    state.store().locations().delete(LocationId(id)).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ----- read only collections --------------------------------------------

async fn list_scripts(State(state): State<AppState>, ApiUser(_): ApiUser) -> ApiResult<Vec<Script>> {
    Ok(Json(state.store().scripts().list().await?))
}

async fn list_users(State(state): State<AppState>, ApiUser(user): ApiUser) -> ApiResult<Vec<User>> {
    Access::new(&state, &user).require_admin()?;
    Ok(Json(state.store().users().list().await?))
}

#[derive(Serialize)]
struct QueueView {
    job_id: JobId,
    client_id: ClientId,
    job_name: String,
    client_name: String,
    state: elkar_core::queue::QueueState,
    enqueued_at: chrono::DateTime<chrono::Utc>,
    running_since: Option<chrono::DateTime<chrono::Utc>>,
    aborted: bool,
}

async fn queue(State(state): State<AppState>, ApiUser(user): ApiUser) -> ApiResult<Vec<QueueView>> {
    let visible: Vec<ClientId> = Access::new(&state, &user)
        .clients(None)
        .await?
        .iter()
        .map(|c| c.id)
        .collect();
    let entries = state.store().queue().list().await?;
    Ok(Json(
        entries
            .into_iter()
            .filter(|e| visible.contains(&e.client_id))
            .map(|e| QueueView {
                job_id: e.item.job_id,
                client_id: e.client_id,
                job_name: e.job_name,
                client_name: e.client_name,
                state: e.item.state,
                enqueued_at: e.item.enqueued_at,
                running_since: e.item.running_since,
                aborted: e.item.aborted,
            })
            .collect(),
    ))
}

#[derive(Deserialize)]
struct LogQuery {
    page: Option<u32>,
    per_page: Option<u32>,
    client: Option<i64>,
    job: Option<i64>,
}

#[derive(Serialize)]
struct LogPage {
    items: Vec<LogRecord>,
    total: i64,
    page: u32,
}

async fn logs(
    State(state): State<AppState>,
    ApiUser(user): ApiUser,
    Query(query): Query<LogQuery>,
) -> ApiResult<LogPage> {
    let filter = Access::new(&state, &user).log_filter(LogFilter {
        client: query.client.map(ClientId),
        job: query.job.map(JobId),
        ..LogFilter::default()
    });
    let page = state
        .store()
        .logs()
        .list(
            &filter,
            query.page.unwrap_or(1),
            query.per_page.unwrap_or(50).clamp(1, 500),
        )
        .await?;
    Ok(Json(LogPage {
        items: page.items,
        total: page.total,
        page: page.page,
    }))
}
