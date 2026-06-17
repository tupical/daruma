//! Error type for the AI crate and its conversion to [`CoreError`].

use taskagent_shared::CoreError;
use thiserror::Error;

/// All failure modes that can occur in the AI crate.
#[derive(Debug, Error)]
pub enum AiError {
    /// A required configuration value was absent.
    #[error("configuration error: {0}")]
    Config(String),

    /// The HTTP transport failed (network, timeout, …).
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    /// The OpenAI API returned a non-2xx status.
    #[error("API error {status}: {message}")]
    Api { status: u16, message: String },

    /// The model's response could not be parsed into the expected shape.
    #[error("parse failed: {0}")]
    ParseFailed(String),

    /// JSON (de)serialisation failed.
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),

    /// The model returned a message without the expected tool call.
    #[error("no tool call in response")]
    NoToolCall,
}

impl From<AiError> for CoreError {
    fn from(e: AiError) -> Self {
        CoreError::ai(e.to_string())
    }
}
