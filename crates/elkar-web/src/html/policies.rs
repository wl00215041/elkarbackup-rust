use std::collections::HashMap;

use askama::Template;
use axum::Router;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum_extra::extract::Form;
use elkar_core::ValidationErrors;
use elkar_core::ids::PolicyId;
use elkar_core::policy::{Policy, PolicySpec, RetainLevel, RetainRule};
use elkar_core::schedule::{Schedule, parse};

use super::{FormErrors, Layout, field, render, split_validation};
use crate::AppState;
use crate::auth::CurrentUser;
use crate::error::AppResult;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/policies", get(list))
        .route("/policy/new", get(new_form).post(create))
        .route("/policy/{id}", get(edit_form).post(update))
        .route("/policy/{id}/delete", post(delete))
}

struct PolicyRow {
    id: i64,
    name: String,
    description: String,
    retains: String,
}

#[derive(Template)]
#[template(path = "policies.html")]
struct PoliciesPage {
    layout: Layout,
    policies: Vec<PolicyRow>,
}

fn describe(policy: &Policy) -> String {
    policy
        .retains()
        .iter()
        .map(|r| format!("{} ×{} at {}", r.level, r.count, parse::join_times(&r.schedule.times)))
        .collect::<Vec<_>>()
        .join("; ")
}

async fn list(State(state): State<AppState>, current: CurrentUser) -> AppResult<Response> {
    current.require_admin()?;
    let policies = state
        .store()
        .policies()
        .list()
        .await?
        .into_iter()
        .map(|p| PolicyRow {
            id: p.id.0,
            retains: describe(&p),
            name: p.spec.name,
            description: p.spec.description,
        })
        .collect();
    render(&PoliciesPage {
        layout: Layout::new(&state, &current, "policies").await,
        policies,
    })
}

/// One retain level row of the policy form.
struct RetainRow {
    level: &'static str,
    enabled: bool,
    count: String,
    times: String,
    weekdays: String,
    days: String,
    months: String,
}

#[derive(Default)]
struct PolicyForm {
    name: String,
    description: String,
    include: String,
    exclude: String,
    sync_first: bool,
    rows: Vec<RetainRow>,
}

impl PolicyForm {
    fn from_policy(policy: Option<&Policy>) -> Self {
        let rules: HashMap<RetainLevel, &RetainRule> = policy
            .map(|p| p.retains().iter().map(|r| (r.level, r)).collect())
            .unwrap_or_default();
        let rows = RetainLevel::ALL
            .iter()
            .map(|level| match rules.get(level) {
                Some(rule) => RetainRow {
                    level: level.as_str(),
                    enabled: true,
                    count: rule.count.to_string(),
                    times: parse::join_times(&rule.schedule.times),
                    weekdays: parse::join(rule.schedule.days_of_week.iter().map(|d| d.to_string().to_lowercase())),
                    days: parse::join(&rule.schedule.days_of_month),
                    months: parse::join(&rule.schedule.months),
                },
                None => RetainRow {
                    level: level.as_str(),
                    enabled: false,
                    count: String::new(),
                    times: String::new(),
                    weekdays: String::new(),
                    days: String::new(),
                    months: String::new(),
                },
            })
            .collect();
        match policy {
            Some(p) => Self {
                name: p.spec.name.clone(),
                description: p.spec.description.clone(),
                include: p.spec.include.clone(),
                exclude: p.spec.exclude.clone(),
                sync_first: p.spec.sync_first,
                rows,
            },
            None => Self {
                rows,
                ..Self::default()
            },
        }
    }

    /// Reads the flat `<level>_<field>` inputs.
    fn from_fields(fields: &[(String, String)]) -> Self {
        let get = |key: &str| {
            fields
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.clone())
                .unwrap_or_default()
        };
        let rows = RetainLevel::ALL
            .iter()
            .map(|level| {
                let key = |f: &str| format!("{level}_{f}");
                RetainRow {
                    level: level.as_str(),
                    enabled: get(&key("enabled")) == "true",
                    count: get(&key("count")),
                    times: get(&key("times")),
                    weekdays: get(&key("weekdays")),
                    days: get(&key("days")),
                    months: get(&key("months")),
                }
            })
            .collect();
        Self {
            name: get("name"),
            description: get("description"),
            include: get("include"),
            exclude: get("exclude"),
            sync_first: get("sync_first") == "true",
            rows,
        }
    }

    fn to_spec(&self) -> Result<PolicySpec, ValidationErrors> {
        let mut errors = ValidationErrors::new();
        let mut retains = Vec::new();
        for (row, level) in self.rows.iter().zip(RetainLevel::ALL) {
            if !row.enabled {
                continue;
            }
            let level_name = level.as_str();
            let times = collect(&mut errors, level_name, parse::times(&row.times));
            let days_of_week = collect(&mut errors, level_name, parse::weekdays(&row.weekdays));
            let days_of_month = collect(&mut errors, level_name, parse::numbers(&row.days));
            let months = collect(&mut errors, level_name, parse::numbers(&row.months));
            let schedule = match (times, days_of_week, days_of_month, months) {
                (Some(times), Some(days_of_week), Some(days_of_month), Some(months)) => Some(Schedule {
                    times,
                    days_of_week,
                    days_of_month,
                    months,
                }),
                _ => None,
            };
            let count = field::number(&mut errors, "retains", &row.count, 0);
            if let Some(schedule) = schedule {
                retains.push(RetainRule {
                    level: *level,
                    count,
                    schedule,
                });
            }
        }
        errors.into_result()?;
        Ok(PolicySpec {
            name: self.name.clone(),
            description: self.description.clone(),
            include: self.include.replace("\r\n", "\n"),
            exclude: self.exclude.replace("\r\n", "\n"),
            sync_first: self.sync_first,
            retains,
        })
    }
}

/// Records a parse failure under `retains` and returns the parsed value.
fn collect<T>(errors: &mut ValidationErrors, level: &str, result: Result<T, String>) -> Option<T> {
    result.map_err(|e| errors.add("retains", format!("{level}: {e}"))).ok()
}

#[derive(Template)]
#[template(path = "policy_form.html")]
struct PolicyPage {
    layout: Layout,
    id: Option<i64>,
    form: PolicyForm,
    errors: FormErrors,
}

async fn page(
    state: &AppState,
    current: &CurrentUser,
    id: Option<PolicyId>,
    form: PolicyForm,
    errors: FormErrors,
) -> AppResult<Response> {
    render(&PolicyPage {
        layout: Layout::new(state, current, "policies").await,
        id: id.map(|i| i.0),
        form,
        errors,
    })
}

async fn new_form(State(state): State<AppState>, current: CurrentUser) -> AppResult<Response> {
    current.require_admin()?;
    page(
        &state,
        &current,
        None,
        PolicyForm::from_policy(None),
        FormErrors::default(),
    )
    .await
}

async fn edit_form(State(state): State<AppState>, current: CurrentUser, Path(id): Path<i64>) -> AppResult<Response> {
    current.require_admin()?;
    let policy = state.store().policies().get(PolicyId(id)).await?;
    page(
        &state,
        &current,
        Some(policy.id),
        PolicyForm::from_policy(Some(&policy)),
        FormErrors::default(),
    )
    .await
}

async fn save(
    state: AppState,
    current: CurrentUser,
    id: Option<PolicyId>,
    fields: Vec<(String, String)>,
) -> AppResult<Response> {
    current.require_admin()?;
    let form = PolicyForm::from_fields(&fields);
    let store = state.store();
    let result = async {
        let spec = form.to_spec()?.validate()?;
        match id {
            Some(id) => {
                store.policies().update(id, &spec).await?;
                Ok(id)
            }
            None => Ok(store.policies().create(&spec).await?),
        }
    }
    .await;
    match split_validation(result)? {
        Ok(_) => {
            current.flash(&state, "Policy saved").await;
            Ok(Redirect::to("/policies").into_response())
        }
        Err(errors) => page(&state, &current, id, form, errors).await,
    }
}

async fn create(
    State(state): State<AppState>,
    current: CurrentUser,
    Form(fields): Form<Vec<(String, String)>>,
) -> AppResult<Response> {
    save(state, current, None, fields).await
}

async fn update(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Form(fields): Form<Vec<(String, String)>>,
) -> AppResult<Response> {
    save(state, current, Some(PolicyId(id)), fields).await
}

async fn delete(State(state): State<AppState>, current: CurrentUser, Path(id): Path<i64>) -> AppResult<Response> {
    current.require_admin()?;
    match state.store().policies().delete(PolicyId(id)).await {
        Ok(()) => current.flash(&state, "Policy deleted").await,
        Err(elkar_storage::StorageError::InUse(_)) => {
            current
                .flash(&state, "The policy is used by jobs and cannot be deleted")
                .await;
        }
        Err(other) => return Err(other.into()),
    }
    Ok(Redirect::to("/policies").into_response())
}
