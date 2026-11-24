use std::fmt;

/// A single invalid field in user supplied input.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ValidationError {
    pub field: &'static str,
    pub message: String,
}

/// Every validation problem found in one input, so a form can show them all at once.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, thiserror::Error)]
pub struct ValidationErrors(Vec<ValidationError>);

impl ValidationErrors {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, field: &'static str, message: impl Into<String>) {
        self.0.push(ValidationError {
            field,
            message: message.into(),
        });
    }

    /// Records an error when `condition` does not hold.
    pub fn check(&mut self, condition: bool, field: &'static str, message: &str) {
        if !condition {
            self.add(field, message);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &ValidationError> {
        self.0.iter()
    }

    /// Converts the accumulated errors into a `Result`.
    pub fn into_result(self) -> Result<(), Self> {
        if self.is_empty() { Ok(()) } else { Err(self) }
    }

    pub fn single(field: &'static str, message: impl Into<String>) -> Self {
        let mut errors = Self::new();
        errors.add(field, message);
        errors
    }
}

impl fmt::Display for ValidationErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let parts: Vec<String> = self.0.iter().map(|e| format!("{}: {}", e.field, e.message)).collect();
        f.write_str(&parts.join("; "))
    }
}

/// A stored or submitted string that does not name any variant of an enum.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown {type_name} value: {value:?}")]
pub struct UnknownVariant {
    pub type_name: &'static str,
    pub value: String,
}

/// Returns true when `value` contains characters that would break out of a
/// single rsnapshot config line or a shell argument.
pub(crate) fn has_line_break(value: &str) -> bool {
    value.contains(['\n', '\r'])
}
