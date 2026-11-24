use askama::Template;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use elkar_core::ValidationErrors;
use elkar_core::ids::UserId;
use elkar_core::user::{Role, UserSpec};
use serde::Deserialize;

use super::{FormErrors, Layout, field, render, split_validation};
use crate::AppState;
use crate::access::Access;
use crate::auth::CurrentUser;
use crate::error::{AppError, AppResult};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/users", get(list))
        .route("/user/new", get(new_form).post(create))
        .route("/user/{id}", get(edit_form).post(update))
        .route("/user/{id}/delete", post(delete))
}

struct UserRow {
    id: i64,
    username: String,
    email: String,
    role: &'static str,
    is_active: bool,
}

#[derive(Template)]
#[template(path = "users.html")]
struct UsersPage {
    layout: Layout,
    users: Vec<UserRow>,
}

async fn list(State(state): State<AppState>, current: CurrentUser) -> AppResult<Response> {
    current.require_admin()?;
    let users = state
        .store()
        .users()
        .list()
        .await?
        .into_iter()
        .map(|u| UserRow {
            id: u.id.0,
            role: u.spec.role.as_str(),
            is_active: u.spec.is_active,
            username: u.spec.username,
            email: u.spec.email,
        })
        .collect();
    render(&UsersPage {
        layout: Layout::new(&state, &current, "users").await,
        users,
    })
}

#[derive(Deserialize, Default)]
struct UserForm {
    username: String,
    #[serde(default)]
    email: String,
    #[serde(default)]
    role: String,
    #[serde(default)]
    is_active: bool,
    #[serde(default)]
    lines_per_page: String,
    #[serde(default)]
    password: String,
}

impl UserForm {
    fn to_spec(&self) -> Result<UserSpec, ValidationErrors> {
        let mut errors = ValidationErrors::new();
        let lines_per_page = field::number(&mut errors, "lines_per_page", &self.lines_per_page, 20);
        let role = self.role.parse::<Role>().unwrap_or_else(|_| {
            errors.add("role", "unknown role");
            Role::User
        });
        errors.into_result()?;
        Ok(UserSpec {
            username: self.username.clone(),
            email: self.email.clone(),
            role,
            is_active: self.is_active,
            lines_per_page,
        })
    }
}

#[derive(Template)]
#[template(path = "user_form.html")]
struct UserPage {
    layout: Layout,
    id: Option<i64>,
    form: UserForm,
    errors: FormErrors,
}

async fn page(
    state: &AppState,
    current: &CurrentUser,
    id: Option<UserId>,
    mut form: UserForm,
    errors: FormErrors,
) -> AppResult<Response> {
    form.password.clear();
    render(&UserPage {
        layout: Layout::new(state, current, "users").await,
        id: id.map(|i| i.0),
        form,
        errors,
    })
}

async fn new_form(State(state): State<AppState>, current: CurrentUser) -> AppResult<Response> {
    current.require_admin()?;
    let form = UserForm {
        role: Role::User.as_str().into(),
        is_active: true,
        lines_per_page: "20".into(),
        ..UserForm::default()
    };
    page(&state, &current, None, form, FormErrors::default()).await
}

async fn edit_form(State(state): State<AppState>, current: CurrentUser, Path(id): Path<i64>) -> AppResult<Response> {
    current.require_admin()?;
    let user = state.store().users().get(UserId(id)).await?;
    let form = UserForm {
        username: user.spec.username,
        email: user.spec.email,
        role: user.spec.role.as_str().into(),
        is_active: user.spec.is_active,
        lines_per_page: user.spec.lines_per_page.to_string(),
        password: String::new(),
    };
    page(&state, &current, Some(user.id), form, FormErrors::default()).await
}

async fn save(state: AppState, current: CurrentUser, id: Option<UserId>, form: UserForm) -> AppResult<Response> {
    let result = match form.to_spec() {
        Ok(spec) => {
            Access::new(&state, &current.user)
                .save_user(id, spec, Some(form.password.clone()))
                .await
        }
        Err(errors) => Err(errors.into()),
    };
    match split_validation(result)? {
        Ok(_) => {
            current.flash(&state, "User saved").await;
            Ok(Redirect::to("/users").into_response())
        }
        Err(errors) => page(&state, &current, id, form, errors).await,
    }
}

async fn create(
    State(state): State<AppState>,
    current: CurrentUser,
    Form(form): Form<UserForm>,
) -> AppResult<Response> {
    save(state, current, None, form).await
}

async fn update(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Form(form): Form<UserForm>,
) -> AppResult<Response> {
    save(state, current, Some(UserId(id)), form).await
}

async fn delete(State(state): State<AppState>, current: CurrentUser, Path(id): Path<i64>) -> AppResult<Response> {
    match Access::new(&state, &current.user).delete_user(UserId(id)).await {
        Ok(()) => current.flash(&state, "User deleted").await,
        Err(AppError::Conflict(message)) => current.flash(&state, message).await,
        Err(other) => return Err(other),
    }
    Ok(Redirect::to("/users").into_response())
}
