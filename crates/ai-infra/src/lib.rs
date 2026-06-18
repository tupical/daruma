//! `taskagent-ai-infra` — provider-neutral AI infrastructure.
//!
//! This crate holds the parts of the AI layer that do **not** depend on
//! TaskAgent's task operations: the OpenAI Responses API client, runtime
//! config, the [`AiProvider`] abstraction, the prompt **rendering engine**,
//! the function-tool JSON schemas, and prompt-injection hardening.
//!
//! # Primitive/product boundary
//!
//! `taskagent-ai-infra` is the **primitive** layer — shared infrastructure
//! that lives in the OSS core and is consumed by upper layers via
//! `vendor/oss`. It has no knowledge of task operations.
//!
//! AI operations (parse, decompose, scope, research, complexity) are
//! **product** concerns that live in the upper layers:
//! `intake_oss`, `sensemaking_oss`, `planning_oss`. They depend on this
//! crate through `vendor/oss/crates/ai-infra`, never the reverse.
//!
//! `taskagent-ai` (same repo) holds the one remaining core operation
//! (`analyze_complexity`) plus the operation prompt catalogue. Upper
//! layers that once imported parse/decompose/scope/research from here
//! should source those from their own layer crates instead.
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
