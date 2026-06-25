//! `daruma-ai` — NL→Command operations on top of [`daruma_ai_infra`].
//!
//! The provider-neutral infrastructure (Responses API client, config,
//! [`AiProvider`] abstraction, prompt rendering engine, tool schemas,
//! prompt-injection hardening) lives in `daruma-ai-infra`. This crate
//! holds the task operations — currently analyze-complexity — that turn
//! model output into [`daruma_core::Command`]s or plain strings, plus
//! the operation prompt catalogue ([`prompts`]) those operations render.
//!
//! # Contract
//! - The AI layer **never** writes to storage. Every output is a
//!   [`daruma_core::Command`] or a plain `String`.
//! - All JSON is built with [`serde_json::json!`]; no string concatenation.
//! - Errors propagate as [`daruma_shared::CoreError`].
//!
//! # Quick start
//! ```no_run
//! use daruma_ai::{AiConfig, OpenAiClient, analyze_complexity_batch};
//!
//! # async fn example() -> daruma_shared::Result<()> {
//! let config = AiConfig::from_env()?;
//! let client = OpenAiClient::new(config);
//! // let result = analyze_complexity_batch(&client, tasks).await?;
//! # Ok(())
//! # }
//! ```

pub mod analyze_complexity;
pub mod prompts;

// ── Re-export the infrastructure layer ─────────────────────────────────────────
//
// Preserves `daruma-ai`'s public surface (`OpenAiClient`, `AiConfig`,
// `AiProvider`, …) so existing consumers (server, mcp, desktop) keep
// compiling against `daruma_ai::*`.
pub use daruma_ai_infra::{wrap_untrusted, AiConfig, AiError, AiProvider, OpenAiClient};

// `PromptRegistry` is the operation prompt catalogue, owned by this crate.
pub use prompts::PromptRegistry;

// ── Operation re-exports ────────────────────────────────────────────────────────

pub use analyze_complexity::{analyze_complexity_batch, MAX_BATCH_TASKS};
