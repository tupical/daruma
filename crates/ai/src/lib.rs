//! `taskagent-ai` — NL→Command operations on top of [`taskagent_ai_infra`].
//!
//! The provider-neutral infrastructure (Responses API client, config,
//! [`AiProvider`] abstraction, prompt rendering engine, tool schemas,
//! prompt-injection hardening) lives in `taskagent-ai-infra`. This crate
//! holds the task operations — parse, research, analyze-complexity,
//! suggest, summarize — that turn model output into
//! [`taskagent_core::Command`]s or plain strings, plus the operation
//! prompt catalogue ([`prompts`]) those operations render.
//!
//! # Contract
//! - The AI layer **never** writes to storage. Every output is a
//!   [`taskagent_core::Command`] or a plain `String`.
//! - All JSON is built with [`serde_json::json!`]; no string concatenation.
//! - Errors propagate as [`taskagent_shared::CoreError`].
//!
//! # Quick start
//! ```no_run
//! use taskagent_ai::{AiConfig, OpenAiClient, parse_task};
//!
//! # async fn example() -> taskagent_shared::Result<()> {
//! let config = AiConfig::from_env()?;
//! let client = OpenAiClient::new(config);
//! let cmd = parse_task(&client, "Remind me to buy milk tomorrow").await?;
//! # Ok(())
//! # }
//! ```

pub mod analyze_complexity;
pub mod parse;
pub mod prompts;
pub mod research;
pub mod suggest;
pub mod summarize;

// ── Re-export the infrastructure layer ─────────────────────────────────────────
//
// Preserves `taskagent-ai`'s public surface (`OpenAiClient`, `AiConfig`,
// `AiProvider`, …) so existing consumers (server, mcp, desktop) keep
// compiling against `taskagent_ai::*`.
pub use taskagent_ai_infra::{wrap_untrusted, AiConfig, AiError, AiProvider, OpenAiClient};

// `PromptRegistry` is the operation prompt catalogue, owned by this crate.
pub use prompts::PromptRegistry;

// ── Operation re-exports ────────────────────────────────────────────────────────

pub use analyze_complexity::{analyze_complexity_batch, MAX_BATCH_TASKS};
pub use parse::parse_task;
pub use suggest::suggest_next_action;
pub use summarize::summarize_project;
