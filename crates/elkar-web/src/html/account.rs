use askama::Template;
use axum::extract::State;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use axum_extra::extract::CookieJar;
use axum_extra::extract::cookie::Cookie;
use elkar_core::log::{LogLevel, NewLogRecord};
use elkar_core::user::UserSpec;
use serde::Deserialize;

use super::{FormErrors, Layout, field, render, split_validation};
use crate::AppState;
use crate::access::Access;
use crate::auth::{self, CurrentUser, SESSION_COOKIE};
use crate::error::AppResult;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/login", get(login_form).post(login))
        .route("/logout", post(logout))
        .route("/account", get(account).post(save_preferences))
        .route("/account/password", post(change_password))
}

#[derive(Template)]
#[template(path = "login.html")]
struct LoginPage {
    username: String,
    error: Option<&'static str>,
}

async fn login_form() -> AppResult<Response> {
    render(&LoginPage {
        username: String::new(),
        error: None,
    })
}

#[derive(Deserialize)]
struct LoginForm {
    username: String,
    password: String,
}

async fn login(State(state): State<AppState>, jar: CookieJar, Form(form): Form<LoginForm>) -> AppResult<Response> {
    let Some(user) = auth::authenticate(&state, &form.username, &form.password).await? else {
        state
            .engine
            .activity()
            .record(NewLogRecord::new(
                LogLevel::Warning,
                "Web",
                format!("Failed login attempt for \"{}\"", form.username),
            ))
            .await;
        return render(&LoginPage {
            username: form.username,
            error: Some("Invalid user name or password"),
        });
    };
    let cookie = auth::start_session(&state, &user).await?;
    Ok((jar.add(cookie), Redirect::to("/clients")).into_response())
}

async fn logout(State(state): State<AppState>, jar: CookieJar) -> AppResult<Response> {
    auth::end_session(&state, &jar).await?;
    Ok((
        jar.remove(Cookie::build(SESSION_COOKIE).path("/")),
        Redirect::to("/login"),
    )
        .into_response())
}

#[derive(Template)]
#[template(path = "account.html")]
struct AccountPage {
    layout: Layout,
    email: String,
    lines_per_page: String,
    errors: FormErrors,
}

async fn account(State(state): State<AppState>, current: CurrentUser) -> AppResult<Response> {
    let page = AccountPage {
        layout: Layout::new(&state, &current, "account").await,
        email: current.user.spec.email.clone(),
        lines_per_page: current.user.spec.lines_per_page.to_string(),
        errors: FormErrors::default(),
    };
    render(&page)
}

#[derive(Deserialize)]
struct PreferencesForm {
    email: String,
    lines_per_page: String,
}

async fn save_preferences(
    State(state): State<AppState>,
    current: CurrentUser,
    Form(form): Form<PreferencesForm>,
) -> AppResult<Response> {
    let mut errors = elkar_core::ValidationErrors::new();
    let lines = field::number(&mut errors, "lines_per_page", &form.lines_per_page, 20);
    let spec = UserSpec {
        email: form.email.clone(),
        lines_per_page: lines,
        ..current.user.spec.clone()
    };
    let result = match errors.into_result().and_then(|()| spec.validate()) {
        Ok(spec) => state
            .store()
            .users()
            .update(current.user.id, &spec)
            .await
            .map_err(Into::into),
        Err(errors) => Err(errors.into()),
    };
    match split_validation(result)? {
        Ok(()) => {
            current.flash(&state, "Preferences saved").await;
            Ok(Redirect::to("/account").into_response())
        }
        Err(errors) => render(&AccountPage {
            layout: Layout::new(&state, &current, "account").await,
            email: form.email,
            lines_per_page: form.lines_per_page,
            errors,
        }),
    }
}

#[derive(Deserialize)]
struct PasswordForm {
    old_password: String,
    new_password: String,
    confirm_password: String,
}

async fn change_password(
    State(state): State<AppState>,
    current: CurrentUser,
    Form(form): Form<PasswordForm>,
) -> AppResult<Response> {
    let result = if form.new_password == form.confirm_password {
        Access::new(&state, &current.user)
            .change_own_password(&form.old_password, &form.new_password)
            .await
    } else {
        Err(elkar_core::ValidationErrors::single("confirm_password", "the passwords do not match").into())
    };
    match split_validation(result)? {
        Ok(()) => {
            current.flash(&state, "Password changed").await;
            Ok(Redirect::to("/account").into_response())
        }
        Err(errors) => render(&AccountPage {
            layout: Layout::new(&state, &current, "account").await,
            email: current.user.spec.email.clone(),
            lines_per_page: current.user.spec.lines_per_page.to_string(),
            errors,
        }),
    }
}
