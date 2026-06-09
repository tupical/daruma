//! `taskagent-ai` — OpenAI Responses API client + NL→Command parsers.
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
pub mod client;
pub mod config;
pub mod decompose;
pub mod error;
pub mod parse;
pub mod prompts;
pub mod provider;
pub mod research;
pub mod scope;
pub mod suggest;
pub mod summarize;
pub mod tools;
pub mod untrusted;

// ── Flat re-exports ───────────────────────────────────────────────────────────

pub use analyze_complexity::{analyze_complexity_batch, MAX_BATCH_TASKS};
pub use client::OpenAiClient;
pub use config::AiConfig;
pub use decompose::decompose_task;
pub use error::AiError;
pub use parse::parse_task;
pub use prompts::PromptRegistry;
pub use provider::AiProvider;
pub use scope::{scope_task, ScopeDirection};
pub use suggest::suggest_next_action;
pub use summarize::summarize_project;
pub use untrusted::wrap_untrusted;
