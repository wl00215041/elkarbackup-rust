//! Password hashing, login sessions and the authenticated user extractors.

use std::sync::OnceLock;

use argon2::Argon2;
use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use chrono::Utc;
use elkar_core::user::User;
use rand::RngCore;
use sha2::{Digest, Sha256};

use crate::AppState;
use crate::error::{AppError, AppResult};

pub const SESSION_COOKIE: &str = "elkar_session";

pub fn hash_password(password: &str) -> Result<String, String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| e.to_string())
}

fn verify_password(hash: &str, password: &str) -> bool {
    PasswordHash::new(hash).is_ok_and(|parsed| Argon2::default().verify_password(password.as_bytes(), &parsed).is_ok())
}

/// Verifies credentials, spending the same effort whether or not the user
/// exists so response times do not reveal valid user names.
pub async fn authenticate(state: &AppState, username: &str, password: &str) -> AppResult<Option<User>> {
    static DUMMY: OnceLock<String> = OnceLock::new();
    let found = state.store().users().find_by_username(username).await?;
    let (hash, user) = match found {
        Some(found) => (found.password_hash, Some(found.user)),
        None => (
            DUMMY.get_or_init(|| hash_password("dummy").unwrap_or_default()).clone(),
            None,
        ),
    };
    let password = password.to_owned();
    let valid = tokio::task::spawn_blocking(move || verify_password(&hash, &password))
        .await
        .map_err(AppError::internal)?;
    Ok(user.filter(|u| valid && u.spec.is_active))
}

/// A random URL safe secret (sessions, job trigger tokens).
pub fn random_token() -> String {
    let mut bytes = [0_u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

fn token_hash(token: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(token.as_bytes()))
}

/// Compares secrets without leaking their common prefix length.
pub fn constant_time_eq(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0_u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Creates a session and returns the cookie carrying it.
pub async fn start_session(state: &AppState, user: &User) -> AppResult<Cookie<'static>> {
    let token = random_token();
    let ttl = chrono::Duration::from_std(state.config.session_ttl).map_err(AppError::internal)?;
    state
        .store()
        .sessions()
        .create(&token_hash(&token), user.id, Utc::now() + ttl)
        .await?;
    Ok(Cookie::build((SESSION_COOKIE, token))
        .path("/")
        .http_only(true)
        // Strict same-site cookies are the CSRF defence for form posts.
        .same_site(SameSite::Strict)
        .secure(state.config.secure_cookies)
        .build())
}

pub async fn end_session(state: &AppState, jar: &CookieJar) -> AppResult<()> {
    if let Some(cookie) = jar.get(SESSION_COOKIE) {
        state.store().sessions().delete(&token_hash(cookie.value())).await?;
    }
    Ok(())
}

/// The user behind the session cookie of an HTML request.
pub struct CurrentUser {
    pub user: User,
    session: String,
}

impl CurrentUser {
    pub fn is_admin(&self) -> bool {
        self.user.is_admin()
    }

    pub fn require_admin(&self) -> AppResult<()> {
        if self.is_admin() {
            Ok(())
        } else {
            Err(AppError::Forbidden)
        }
    }

    pub fn name(&self) -> &str {
        &self.user.spec.username
    }

    /// Queues a message for the next rendered page.
    pub async fn flash(&self, state: &AppState, message: impl AsRef<str>) {
        if let Err(err) = state
            .store()
            .sessions()
            .set_flash(&self.session, message.as_ref())
            .await
        {
            tracing::warn!(error = %err, "could not store flash message");
        }
    }

    pub async fn take_flash(&self, state: &AppState) -> Option<String> {
        state.store().sessions().take_flash(&self.session).await.ok().flatten()
    }
}

impl FromRequestParts<AppState> for CurrentUser {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        let jar = CookieJar::from_headers(&parts.headers);
        let token = jar.get(SESSION_COOKIE).ok_or(AppError::Unauthorized)?;
        let session = token_hash(token.value());
        let user_id = state
            .store()
            .sessions()
            .user_for(&session)
            .await?
            .ok_or(AppError::Unauthorized)?;
        let user = state.store().users().get(user_id).await?;
        Ok(Self { user, session })
    }
}

/// Lets handlers serve both logged in and anonymous requests.
impl axum::extract::OptionalFromRequestParts<AppState> for CurrentUser {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Option<Self>, Self::Rejection> {
        match <Self as FromRequestParts<AppState>>::from_request_parts(parts, state).await {
            Ok(user) => Ok(Some(user)),
            Err(AppError::Unauthorized | AppError::NotFound) => Ok(None),
            Err(other) => Err(other),
        }
    }
}

/// Credentials of an `Authorization: Basic` header.
pub fn basic_credentials(parts: &Parts) -> Option<(String, String)> {
    let header = parts.headers.get(axum::http::header::AUTHORIZATION)?.to_str().ok()?;
    let encoded = header.strip_prefix("Basic ")?;
    let decoded = String::from_utf8(STANDARD.decode(encoded.trim()).ok()?).ok()?;
    let (user, password) = decoded.split_once(':')?;
    Some((user.to_owned(), password.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_hash_round_trip() {
        let hash = hash_password("correct horse").unwrap();
        assert!(verify_password(&hash, "correct horse"));
        assert!(!verify_password(&hash, "wrong"));
        assert!(!verify_password("not a hash", "x"));
    }

    #[test]
    fn tokens_are_unique_and_hash_stably() {
        let a = random_token();
        assert_ne!(a, random_token());
        assert_eq!(token_hash(&a), token_hash(&a));
        assert!(constant_time_eq(&a, &a.clone()));
        assert!(!constant_time_eq(&a, "short"));
    }
}
