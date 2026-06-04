use thiserror::Error;

/// Top-level domain error.
///
/// All crates wrap their internal failures into one of these variants
/// using the constructor helpers (`CoreError::storage("...")` etc.).
#[derive(Debug, Error)]
pub enum CoreError {
    #[error("not found: {0}")]
    NotFound(String),

    #[error("validation failed: {0}")]
    Validation(String),

    #[error("conflict: {0}")]
    Conflict(String),

    #[error("storage error: {0}")]
    Storage(String),

    #[error("sync error: {0}")]
    Sync(String),

    #[error("ai error: {0}")]
    Ai(String),

    #[error("serialization error: {0}")]
    Serde(String),

    #[error("io error: {0}")]
    Io(String),

    #[error("unauthorized: {0}")]
    Unauthorized(String),

    #[error("forbidden: {0}")]
    Forbidden(String),

    #[error("quota exceeded for {resource}: current {current}, limit {limit}")]
    QuotaExceeded {
        resource: String,
        limit: i64,
        current: i64,
    },
}

impl CoreError {
    /// Returns a stable, machine-readable error code string for this variant.
    pub fn code(&self) -> &'static str {
        match self {
            CoreError::NotFound(_) => "not_found",
            CoreError::Validation(_) => "validation",
            CoreError::Conflict(_) => "conflict",
            CoreError::Storage(_) => "storage_error",
            CoreError::Sync(_) => "sync_error",
            CoreError::Ai(_) => "ai_unavailable",
            CoreError::Serde(_) => "serialization_error",
            CoreError::Io(_) => "io_error",
            CoreError::Unauthorized(_) => "unauthorized",
            CoreError::Forbidden(_) => "forbidden",
            CoreError::QuotaExceeded { .. } => "quota_exceeded",
        }
    }

    pub fn not_found(msg: impl Into<String>) -> Self {
        Self::NotFound(msg.into())
    }
    pub fn validation(msg: impl Into<String>) -> Self {
        Self::Validation(msg.into())
    }
    pub fn conflict(msg: impl Into<String>) -> Self {
        Self::Conflict(msg.into())
    }
    pub fn storage(msg: impl Into<String>) -> Self {
        Self::Storage(msg.into())
    }
    pub fn sync(msg: impl Into<String>) -> Self {
        Self::Sync(msg.into())
    }
    pub fn ai(msg: impl Into<String>) -> Self {
        Self::Ai(msg.into())
    }
    pub fn serde(msg: impl Into<String>) -> Self {
        Self::Serde(msg.into())
    }
    pub fn io(msg: impl Into<String>) -> Self {
        Self::Io(msg.into())
    }
    pub fn unauthorized(msg: impl Into<String>) -> Self {
        Self::Unauthorized(msg.into())
    }
    pub fn forbidden(msg: impl Into<String>) -> Self {
        Self::Forbidden(msg.into())
    }
    pub fn quota_exceeded(resource: impl Into<String>, limit: i64, current: i64) -> Self {
        Self::QuotaExceeded {
            resource: resource.into(),
            limit,
            current,
        }
    }
}
