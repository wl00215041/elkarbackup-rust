//! Users of the web interface and API.

use serde::{Deserialize, Serialize};

use crate::error::ValidationErrors;
use crate::ids::UserId;
use crate::job::looks_like_email;

text_enum! {
    pub enum Role {
        Admin => "admin",
        User => "user",
    }
}

pub const MIN_PASSWORD_LEN: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserSpec {
    pub username: String,
    pub email: String,
    pub role: Role,
    #[serde(default = "yes")]
    pub is_active: bool,
    #[serde(default = "default_lines")]
    pub lines_per_page: u32,
}

fn yes() -> bool {
    true
}

fn default_lines() -> u32 {
    20
}

impl UserSpec {
    pub fn validate(mut self) -> Result<Self, ValidationErrors> {
        let mut errors = ValidationErrors::new();
        self.username = self.username.trim().to_owned();
        self.email = self.email.trim().to_owned();
        errors.check(
            !self.username.is_empty() && self.username.chars().all(|c| c.is_alphanumeric() || "._-@".contains(c)),
            "username",
            "username is required and may contain letters, digits and . _ - @",
        );
        errors.check(looks_like_email(&self.email), "email", "a valid email is required");
        errors.check(
            (5..=500).contains(&self.lines_per_page),
            "lines_per_page",
            "must be between 5 and 500",
        );
        errors.into_result().map(|()| self)
    }
}

pub fn validate_password(password: &str) -> Result<(), ValidationErrors> {
    let mut errors = ValidationErrors::new();
    errors.check(
        password.chars().count() >= MIN_PASSWORD_LEN,
        "password",
        "password must be at least 8 characters",
    );
    errors.into_result()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct User {
    pub id: UserId,
    #[serde(flatten)]
    pub spec: UserSpec,
}

impl User {
    pub fn is_admin(&self) -> bool {
        self.spec.role == Role::Admin
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_username_and_email() {
        let spec = UserSpec {
            username: "bad name".into(),
            email: "nope".into(),
            role: Role::User,
            is_active: true,
            lines_per_page: 20,
        };
        assert_eq!(spec.validate().unwrap_err().iter().count(), 2);
    }

    #[test]
    fn password_length() {
        assert!(validate_password("short").is_err());
        assert!(validate_password("long enough").is_ok());
    }
}
