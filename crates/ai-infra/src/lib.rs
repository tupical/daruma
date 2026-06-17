//! `taskagent-ai-infra` — provider-neutral AI infrastructure.
//!
//! This crate holds the parts of the AI layer that do **not** depend on
//! TaskAgent's task operations: the OpenAI Responses API client, runtime
//! config, the [`AiProvider`] abstraction, the prompt **rendering engine**,
//! the function-tool JSON schemas, and prompt-injection hardening.
//!
//! The operation layer (`taskagent-ai`: parse, decompose, scope,
//! research, …) and future upper layers depend on this crate so they can
//! share one AI package without pulling in task-core internals. The
//! operation prompt *catalogue* is operational, not infrastructure, so it
//! lives in `taskagent-ai`; only the rendering machinery
//! ([`prompts::PromptFile`] / [`prompts::render_variant`]) ships here.
//!
//! # Contract
//! - This layer **never** writes to storage.
//! - All JSON is built with [`serde_json::json!`]; no string concatenation.
//! - Errors propagate as [`taskagent_shared::CoreError`] (via [`AiError`]).

pub mod client;
pub mod config;
pub mod error;
pub mod prompts;
pub mod provider;
pub mod tools;
pub mod untrusted;

// ── Flat re-exports ───────────────────────────────────────────────────────────

pub use client::{OpenAiClient, ResponseOutput, ResponseRequest, ToolCall};
pub use config::AiConfig;
pub use error::AiError;
pub use prompts::{render_variant, PromptFile};
pub use provider::AiProvider;
pub use untrusted::wrap_untrusted;
