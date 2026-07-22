//! Runtime configuration for the AI crate loaded from environment variables.

use crate::error::AiError;

/// All settings the AI client needs to reach the OpenAI Responses API.
#[derive(Clone, Debug)]
pub struct AiConfig {
    /// OpenAI secret key (`OPENAI_API_KEY`).
    pub api_key: String,
    /// Base URL without trailing slash (`OPENAI_BASE_URL`).
    /// Defaults to `https://api.openai.com/v1`.
    pub base_url: String,
    /// Model identifier (`OPENAI_MODEL`). Defaults to `gpt-4.1`.
    pub model: String,
    /// Cap on response tokens (`OPENAI_MAX_OUTPUT_TOKENS`). Always sent as
    /// `max_output_tokens`: proxy billers (e.g. ProxyAPI) otherwise reserve
    /// the model's maximum for the cost forecast, rejecting cheap calls on a
    /// low balance. `None` falls back to the client default.
    pub max_output_tokens: Option<u32>,
}

impl AiConfig {
    /// Load config from environment. Returns [`AiError::Config`] when a
    /// required variable is missing.
    pub fn from_env() -> Result<Self, AiError> {
        let api_key = std::env::var("OPENAI_API_KEY")
            .map_err(|_| AiError::Config("OPENAI_API_KEY not set".into()))?;

        let base_url =
            std::env::var("OPENAI_BASE_URL").unwrap_or_else(|_| "https://api.openai.com/v1".into());

        let model = std::env::var("OPENAI_MODEL").unwrap_or_else(|_| "gpt-4.1".into());

        let max_output_tokens = std::env::var("OPENAI_MAX_OUTPUT_TOKENS")
            .ok()
            .and_then(|v| v.parse().ok());

        Ok(Self {
            api_key,
            base_url,
            model,
            max_output_tokens,
        })
    }

    /// Build the full Responses API endpoint URL.
    #[inline]
    pub fn responses_url(&self) -> String {
        format!("{}/responses", self.base_url)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn responses_url_is_correct() {
        let cfg = AiConfig {
            api_key: "sk-test".into(),
            base_url: "https://api.openai.com/v1".into(),
            model: "gpt-4.1".into(),
            max_output_tokens: None,
        };
        assert_eq!(cfg.responses_url(), "https://api.openai.com/v1/responses");
    }
}
