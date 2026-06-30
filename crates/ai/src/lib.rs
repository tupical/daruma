//! `daruma-ai` — thin proxy over [`daruma_ai_infra`] + a deprecated
//! planning shim.
//!
//! The provider-neutral infrastructure (Responses API client, config,
//! [`AiProvider`] abstraction, prompt rendering engine, tool schemas,
//! prompt-injection hardening) lives in `daruma-ai-infra` and is simply
//! re-exported here so existing `daruma_ai::*` consumers (server, mcp,
//! desktop) keep compiling.
//!
//! # Layer boundary (execution vs planning)
//! daruma is the **execution** layer. The planning operations that
//! transform raw task text into structure — `decompose`, `scope` and
//! `analyze_complexity` — belong in the open-core **planning** layer
//! (`yatagarasu` / `planning_oss`), not here. `decompose` and `scope`
//! have already moved out; [`analyze_complexity`] remains only as a
//! **deprecated delegation-shim** retained until the cloud cutover wires
//! the server route directly to the planning layer (separate plan). New
//! callers must use `yatagarasu::analyze_complexity_batch`; this crate
//! should converge to a pure re-export of `daruma-ai-infra` once that
//! cutover lands.
//!
//! What survives here today is therefore only: the infra re-exports, the
//! batch [`analyze_complexity`] shim, and the operation prompt catalogue
//! ([`prompts`]) that shim renders.
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
