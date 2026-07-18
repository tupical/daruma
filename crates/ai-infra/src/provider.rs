//! AI provider abstraction (§3.8.9).
//!
//! Wraps a model backend behind a minimal trait so the AI tooling layer
//! can be retargeted later (e.g., to a local Ollama deployment, or to a
//! research-grade provider — see [`use_research_provider`] flag in
//! §3.8.13) without rewriting every call site. Only one impl ships
//! today — [`OpenAiClient`]. The trait sets the surface area future
//! providers must satisfy.
//!
//! Two methods cover the entire AI call surface:
//!
//! - [`AiProvider::generate_text`] — free-form completion, no tools.
//! - [`AiProvider::generate_object`] — structured tool-call completion;
//!   returns the raw JSON arguments of the first matching call.
//!
//! The operations themselves (suggest, summarize, parse, decompose,
//! analyze_complexity) live in the upper-layer repos and, for the
//! deprecated core shim, in `apps/server/src/ai.rs`.
//!
//! [`use_research_provider`]: crate::PromptRegistry

use async_trait::async_trait;
use daruma_shared::CoreError;
use serde_json::Value;

use crate::client::{OpenAiClient, ResponseOutput, ResponseRequest};

#[async_trait]
pub trait AiProvider: Send + Sync {
    /// Free-form text completion. The caller assembles the full prompt
    /// (typically via [`crate::PromptRegistry`]) and receives the first
    /// assistant message verbatim.
    async fn generate_text(&self, prompt: String) -> Result<String, CoreError>;

    /// Structured tool-call completion.
    ///
    /// - `prompt` — the assembled prompt.
    /// - `tools` — JSON-encoded tool schemas advertised to the model.
    /// - `expected_tool` — function name the caller intends to dispatch.
    ///   The first `ToolCall` whose `name == expected_tool` is returned;
    ///   the body is the parsed JSON arguments.
    async fn generate_object(
        &self,
        prompt: String,
        tools: Vec<Value>,
        expected_tool: &str,
    ) -> Result<Value, CoreError>;
}

#[async_trait]
impl AiProvider for OpenAiClient {
    async fn generate_text(&self, prompt: String) -> Result<String, CoreError> {
        let req = ResponseRequest {
            input: Value::String(prompt),
            tools: vec![],
            tool_choice: None,
        };
        let outputs = self.respond(req).await.map_err(CoreError::from)?;
        outputs
            .into_iter()
            .find_map(|o| match o {
                ResponseOutput::Message(text) => Some(text),
                _ => None,
            })
            .ok_or_else(|| CoreError::ai("provider returned no text"))
    }

    async fn generate_object(
        &self,
        prompt: String,
        tools: Vec<Value>,
        expected_tool: &str,
    ) -> Result<Value, CoreError> {
        let req = ResponseRequest {
            input: Value::String(prompt),
            tools,
            tool_choice: Some("required".into()),
        };
        let outputs = self.respond(req).await.map_err(CoreError::from)?;
        let tc = outputs
            .into_iter()
            .find_map(|o| match o {
                ResponseOutput::ToolCall(tc) if tc.name == expected_tool => Some(tc),
                _ => None,
            })
            .ok_or_else(|| CoreError::ai(format!("provider returned no {expected_tool} call")))?;
        serde_json::from_str(&tc.arguments).map_err(|e| CoreError::serde(e.to_string()))
    }
}

/// Test doubles for the [`AiProvider`] trait, shared across crates.
///
/// Gated behind the `testing` feature so downstream crates
/// (upper layers via `vendor/oss`) can reuse [`FakeProvider`] in
/// their own tests without re-implementing it. Always compiled — not
/// `#[cfg(test)]` — because a dependent crate's tests cannot see this
/// crate's `cfg(test)` items.
#[cfg(any(test, feature = "testing"))]
pub mod testing {
    use super::*;
    use std::sync::Mutex;

    /// A fake provider records the inputs it received and returns canned
    /// outputs. Lets call-site tests run without network.
    pub struct FakeProvider {
        pub canned_text: String,
        pub canned_object: Value,
        pub captured_prompts: Mutex<Vec<String>>,
    }

    impl FakeProvider {
        pub fn new(text: impl Into<String>, object: Value) -> Self {
            Self {
                canned_text: text.into(),
                canned_object: object,
                captured_prompts: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl AiProvider for FakeProvider {
        async fn generate_text(&self, prompt: String) -> Result<String, CoreError> {
            self.captured_prompts.lock().unwrap().push(prompt);
            Ok(self.canned_text.clone())
        }

        async fn generate_object(
            &self,
            prompt: String,
            _tools: Vec<Value>,
            _expected_tool: &str,
        ) -> Result<Value, CoreError> {
            self.captured_prompts.lock().unwrap().push(prompt);
            Ok(self.canned_object.clone())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use testing::FakeProvider;

    #[tokio::test]
    async fn fake_provider_round_trips_text() {
        let p = FakeProvider::new("ack", serde_json::json!({}));
        let out = p.generate_text("hello".to_owned()).await.unwrap();
        assert_eq!(out, "ack");
        assert_eq!(p.captured_prompts.lock().unwrap().as_slice(), &["hello"]);
    }

    #[tokio::test]
    async fn fake_provider_round_trips_object() {
        let p = FakeProvider::new("", serde_json::json!({"title": "Buy milk"}));
        let v = p
            .generate_object("prompt".to_owned(), vec![], "create_task")
            .await
            .unwrap();
        assert_eq!(v["title"], "Buy milk");
    }

    /// Trait-object form must work — the type-erased path is the one
    /// future call-site refactors will take.
    #[tokio::test]
    async fn provider_is_object_safe() {
        let p: std::sync::Arc<dyn AiProvider> =
            std::sync::Arc::new(FakeProvider::new("ok", serde_json::json!({})));
        assert_eq!(p.generate_text("x".to_owned()).await.unwrap(), "ok");
    }
}
