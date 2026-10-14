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

use crate::client::{BatchWait, OpenAiClient, ResponseOutput, ResponseRequest, SYSTEM_SEPARATOR};
use crate::config::ApiProtocol;

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

    /// [`Self::generate_object`] with a stable `system` part kept apart from
    /// the per-call `prompt`, so a provider that caches prompt prefixes can
    /// cache it. The default sends both as one prompt, system first — the
    /// same text a provider without a system channel would have seen.
    async fn generate_object_with_system(
        &self,
        system: String,
        prompt: String,
        tools: Vec<Value>,
        expected_tool: &str,
    ) -> Result<Value, CoreError> {
        self.generate_object(
            format!("{system}{SYSTEM_SEPARATOR}{prompt}"),
            tools,
            expected_tool,
        )
        .await
    }

    /// Many independent [`Self::generate_object_with_system`] calls sharing
    /// one `system`, `tools` and `expected_tool` — one-turn mass judgments of
    /// an offline pass (scheduled audit). Item `i` answers `prompts[i]`.
    ///
    /// A provider with a batch API may send them as one batch: cheaper, but it
    /// can take minutes, so interactive paths must not call this. The default
    /// makes the calls one by one, so a provider without batches (and every
    /// test double) answers exactly as before.
    async fn generate_objects_batch(
        &self,
        system: String,
        prompts: Vec<String>,
        tools: Vec<Value>,
        expected_tool: &str,
    ) -> Vec<Result<Value, CoreError>> {
        let mut out = Vec::with_capacity(prompts.len());
        for prompt in prompts {
            out.push(
                self.generate_object_with_system(
                    system.clone(),
                    prompt,
                    tools.clone(),
                    expected_tool,
                )
                .await,
            );
        }
        out
    }
}

#[async_trait]
impl AiProvider for OpenAiClient {
    async fn generate_text(&self, prompt: String) -> Result<String, CoreError> {
        let req = ResponseRequest {
            input: Value::String(prompt),
            tools: vec![],
            tool_choice: None,
            system: None,
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
        self.object_call(None, prompt, tools, expected_tool).await
    }

    async fn generate_object_with_system(
        &self,
        system: String,
        prompt: String,
        tools: Vec<Value>,
        expected_tool: &str,
    ) -> Result<Value, CoreError> {
        self.object_call(Some(system), prompt, tools, expected_tool)
            .await
    }

    /// Anthropic: one Message Batch for two or more prompts. A batch that
    /// fails as a whole (not created, not ended in time, results unreadable)
    /// falls back to the one-by-one calls, so the pass still gets its answers
    /// at the normal price. Other protocols have no batches and call one by one.
    async fn generate_objects_batch(
        &self,
        system: String,
        prompts: Vec<String>,
        tools: Vec<Value>,
        expected_tool: &str,
    ) -> Vec<Result<Value, CoreError>> {
        if self.config().api_protocol == ApiProtocol::AnthropicMessages && prompts.len() > 1 {
            let reqs: Vec<ResponseRequest> = prompts
                .iter()
                .map(|prompt| object_request(Some(system.clone()), prompt.clone(), tools.clone()))
                .collect();
            match self.respond_batch(&reqs, BatchWait::default()).await {
                Ok(results) => {
                    return results
                        .into_iter()
                        .map(|outputs| {
                            tool_arguments(outputs.map_err(CoreError::from)?, expected_tool)
                        })
                        .collect();
                }
                Err(error) => tracing::warn!(
                    %error,
                    prompts = prompts.len(),
                    "anthropic batch failed, falling back to one call per prompt"
                ),
            }
        }
        let mut out = Vec::with_capacity(prompts.len());
        for prompt in prompts {
            out.push(
                self.object_call(Some(system.clone()), prompt, tools.clone(), expected_tool)
                    .await,
            );
        }
        out
    }
}

impl OpenAiClient {
    async fn object_call(
        &self,
        system: Option<String>,
        prompt: String,
        tools: Vec<Value>,
        expected_tool: &str,
    ) -> Result<Value, CoreError> {
        let outputs = self
            .respond(object_request(system, prompt, tools))
            .await
            .map_err(CoreError::from)?;
        tool_arguments(outputs, expected_tool)
    }
}

/// The request of one structured tool-call completion.
fn object_request(system: Option<String>, prompt: String, tools: Vec<Value>) -> ResponseRequest {
    ResponseRequest {
        input: Value::String(prompt),
        tools,
        tool_choice: Some("required".into()),
        system,
    }
}

/// Parsed arguments of the first `expected_tool` call among the outputs.
fn tool_arguments(outputs: Vec<ResponseOutput>, expected_tool: &str) -> Result<Value, CoreError> {
    let tc = outputs
        .into_iter()
        .find_map(|o| match o {
            ResponseOutput::ToolCall(tc) if tc.name == expected_tool => Some(tc),
            _ => None,
        })
        .ok_or_else(|| CoreError::ai(format!("provider returned no {expected_tool} call")))?;
    serde_json::from_str(&tc.arguments).map_err(|e| CoreError::serde(e.to_string()))
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

    /// The default keeps every provider without a system channel working:
    /// it sees the same text, system part first.
    #[tokio::test]
    async fn default_generate_object_with_system_prepends_system() {
        let p = FakeProvider::new("", serde_json::json!({"ok": true}));
        let v = p
            .generate_object_with_system("RULES".into(), "per-call".into(), vec![], "report")
            .await
            .unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(
            p.captured_prompts.lock().unwrap().as_slice(),
            &["RULES\n\nper-call"]
        );
    }

    #[tokio::test]
    async fn default_generate_objects_batch_calls_one_by_one_in_order() {
        let p = FakeProvider::new("", serde_json::json!({"ok": true}));
        let results = p
            .generate_objects_batch(
                "RULES".into(),
                vec!["a".into(), "b".into()],
                vec![],
                "report",
            )
            .await;
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|r| r.as_ref().unwrap()["ok"] == true));
        assert_eq!(
            p.captured_prompts.lock().unwrap().as_slice(),
            &["RULES\n\na", "RULES\n\nb"]
        );
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
