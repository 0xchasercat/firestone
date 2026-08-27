use std::error::Error;

use serde::{Deserialize, Serialize};

/// Stable machine-readable categories shared by every Firestone interface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ErrorKind {
    Generic,
    Usage,
    InvalidSpec,
    NotFound,
    NotRunning,
    Conflict,
    AlreadyExists,
    AlreadyRunning,
    Busy,
    Dependency,
    Timeout,
    Checksum,
    Interrupted,
}

impl ErrorKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Generic => "generic",
            Self::Usage => "usage",
            Self::InvalidSpec => "invalid_spec",
            Self::NotFound => "not_found",
            Self::NotRunning => "not_running",
            Self::Conflict => "conflict",
            Self::AlreadyExists => "already_exists",
            Self::AlreadyRunning => "already_running",
            Self::Busy => "busy",
            Self::Dependency => "dependency",
            Self::Timeout => "timeout",
            Self::Checksum => "checksum",
            Self::Interrupted => "interrupted",
        }
    }
}

impl std::fmt::Display for ErrorKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Serializable error data used by events, JSON output, and the REST API.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorInfo {
    pub kind: ErrorKind,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

/// The error type returned by core operations.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct FirestoneError {
    kind: ErrorKind,
    message: String,
    hint: Option<String>,
    #[source]
    source: Option<Box<dyn Error + Send + Sync + 'static>>,
}

impl FirestoneError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            hint: None,
            source: None,
        }
    }

    #[must_use]
    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    #[must_use]
    pub fn with_source(mut self, source: impl Error + Send + Sync + 'static) -> Self {
        self.source = Some(Box::new(source));
        self
    }

    #[must_use]
    pub const fn kind(&self) -> ErrorKind {
        self.kind
    }

    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    #[must_use]
    pub fn hint(&self) -> Option<&str> {
        self.hint.as_deref()
    }

    #[must_use]
    pub fn info(&self) -> ErrorInfo {
        ErrorInfo::from(self)
    }
}

impl From<&FirestoneError> for ErrorInfo {
    fn from(error: &FirestoneError) -> Self {
        Self {
            kind: error.kind,
            message: error.message.clone(),
            hint: error.hint.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::{ErrorInfo, ErrorKind, FirestoneError};

    #[test]
    fn error_kind_serialization_uses_stable_name() -> Result<(), serde_json::Error> {
        let serialized = serde_json::to_string(&ErrorKind::AlreadyRunning)?;

        assert_eq!(serialized, r#""already_running""#);
        Ok(())
    }

    #[test]
    fn error_with_context_preserves_public_info_and_source() {
        let error = FirestoneError::new(ErrorKind::Dependency, "cannot open /dev/kvm")
            .with_hint("run firestone doctor")
            .with_source(io::Error::from(io::ErrorKind::PermissionDenied));

        assert_eq!(error.kind(), ErrorKind::Dependency);
        assert_eq!(error.message(), "cannot open /dev/kvm");
        assert_eq!(error.hint(), Some("run firestone doctor"));
        assert!(std::error::Error::source(&error).is_some());
        assert_eq!(
            error.info(),
            ErrorInfo {
                kind: ErrorKind::Dependency,
                message: "cannot open /dev/kvm".to_owned(),
                hint: Some("run firestone doctor".to_owned()),
            }
        );
    }
}
