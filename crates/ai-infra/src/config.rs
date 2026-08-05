//! Runtime configuration for the AI crate loaded from environment variables.

use std::str::FromStr;

use crate::error::AiError;

/// Wire protocol used by an OpenAI-compatible provider.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ApiProtocol {
    #[default]
    Responses,
    ChatCompletions,
}

impl FromStr for ApiProtocol {
    type Err = AiError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "responses" => Ok(Self::Responses),
            "chat_completions" => Ok(Self::ChatCompletions),
            _ => Err(AiError::Config(format!(
                "OPENAI_API_PROTOCOL must be 'responses' or 'chat_completions', got '{value}'"
            ))),
        }
    }
}

/// All settings the AI client needs to reach an OpenAI-compatible API.
#[derive(Clone, Debug)]
pub struct AiConfig {
    /// OpenAI secret key (`OPENAI_API_KEY`).
    pub api_key: String,
    /// Base URL without trailing slash (`OPENAI_BASE_URL`).
    /// Defaults to `https://api.openai.com/v1`.
    pub base_url: String,
    /// Model identifier (`OPENAI_MODEL`). Defaults to `gpt-4.1`.
    pub model: String,
    /// Provider wire protocol (`OPENAI_API_PROTOCOL`). Defaults to Responses.
    pub api_protocol: ApiProtocol,
    /// Reasoning budget for models that expose one (`OPENAI_REASONING_EFFORT`).
    ///
    /// `None` sends nothing, which is the only safe default: providers reject
    /// parameters they do not know, so this must stay opt-in per workspace. Set
    /// it where the model reasons by default — Kimi K3 defaults to `max`, and at
    /// `max` it spends an entire token budget thinking about a yes/no question
    /// and never reaches the tool call.
    pub reasoning_effort: Option<String>,
    /// Cap on response tokens (`OPENAI_MAX_OUTPUT_TOKENS`). Always sent as
    /// `max_output_tokens` (Responses) or `max_tokens` (Chat Completions):
    /// proxy billers otherwise reserve the model's maximum for the cost
    /// forecast, rejecting cheap calls on a low balance. `None` uses the client
    /// default.
    pub max_output_tokens: Option<u32>,
    /// Whole-request timeout selected for this provider profile.
    pub request_timeout_seconds: Option<u64>,
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

        let api_protocol = std::env::var("OPENAI_API_PROTOCOL")
            .unwrap_or_else(|_| "responses".into())
            .parse()?;

        let reasoning_effort = std::env::var("OPENAI_REASONING_EFFORT")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty());

        let max_output_tokens = std::env::var("OPENAI_MAX_OUTPUT_TOKENS")
            .ok()
            .and_then(|v| v.parse().ok());
        let request_timeout_seconds = std::env::var("OPENAI_REQUEST_TIMEOUT_SECONDS")
            .ok()
            .and_then(|v| v.parse().ok());

        Ok(Self {
            api_key,
            base_url,
            model,
            api_protocol,
            reasoning_effort,
            max_output_tokens,
            request_timeout_seconds,
        })
    }

    /// Build the full Responses API endpoint URL.
    #[inline]
    pub fn responses_url(&self) -> String {
        format!("{}/responses", self.base_url)
    }

    /// Build the full Chat Completions API endpoint URL.
    #[inline]
    pub fn chat_completions_url(&self) -> String {
        format!("{}/chat/completions", self.base_url)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_defaults_to_responses() {
        assert_eq!(ApiProtocol::default(), ApiProtocol::Responses);
    }

    #[test]
    fn protocol_parses_both_arms_and_rejects_the_rest() {
        // The whole point of the enum is the ChatCompletions arm. Without this
        // assert, a typo there ("chat-completions") would silently leave every
        // workspace on Responses — the exact production outage this shipped to
        // fix — with a fully green suite.
        assert_eq!(
            "chat_completions".parse::<ApiProtocol>().unwrap(),
            ApiProtocol::ChatCompletions
        );
        assert_eq!(
            "responses".parse::<ApiProtocol>().unwrap(),
            ApiProtocol::Responses
        );
        for bad in ["chat-completions", "ChatCompletions", "", "chat"] {
            assert!(
                bad.parse::<ApiProtocol>().is_err(),
                "{bad:?} must not parse"
            );
        }
    }

    #[test]
    fn responses_url_is_correct() {
        let cfg = AiConfig {
            api_key: "sk-test".into(),
            base_url: "https://api.openai.com/v1".into(),
            model: "gpt-4.1".into(),
            api_protocol: ApiProtocol::Responses,
            reasoning_effort: None,
            max_output_tokens: None,
            request_timeout_seconds: None,
        };
        assert_eq!(cfg.responses_url(), "https://api.openai.com/v1/responses");
        assert_eq!(
            cfg.chat_completions_url(),
            "https://api.openai.com/v1/chat/completions"
        );
    }
}
