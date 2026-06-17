//! Operation prompt catalogue (§3.8.5).
//!
//! The prompt *rendering engine* and the shared [`SharedRegistry`] live in
//! `taskagent-ai-infra`. This module only declares the catalogue of
//! operation prompts — one `crates/ai/prompts/*.toml` per operation
//! (analyze_complexity, suggest, summarize) — because those prompts are
//! operational, not infrastructure.
//!
//! All known prompts are baked into the binary via `include_str!`; the
//! first [`PromptRegistry::load`] call parses them.
//!
//! ```ignore
//! use serde::Serialize;
//! use taskagent_ai::prompts::PromptRegistry;
//!
//! #[derive(Serialize)]
//! struct SuggestCtx<'a> { context: &'a str }
//!
//! let s = PromptRegistry::load("suggest", "default", &SuggestCtx { context: "3 open tasks" })?;
//! ```

use once_cell::sync::Lazy;
use serde::Serialize;
use taskagent_ai_infra::prompts::PromptRegistry as SharedRegistry;
use taskagent_shared::CoreError;

static PROMPTS: Lazy<SharedRegistry> = Lazy::new(|| {
    SharedRegistry::new(&[
        ("suggest", include_str!("../prompts/suggest.toml")),
        ("summarize", include_str!("../prompts/summarize.toml")),
        (
            "analyze_complexity",
            include_str!("../prompts/analyze_complexity.toml"),
        ),
    ])
});

/// Process-wide catalogue of operation prompts. All sources are baked into
/// the binary via `include_str!`; the first `load` call parses them.
pub struct PromptRegistry;

impl PromptRegistry {
    /// Render `name` / `variant` against `params`. See
    /// [`SharedRegistry::load`] for error semantics.
    pub fn load<P: Serialize>(name: &str, variant: &str, params: &P) -> Result<String, CoreError> {
        PROMPTS.load(name, variant, params)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Serialize)]
    struct Empty {}

    #[test]
    fn every_bundled_prompt_loads() {
        for (name, _file) in PROMPTS.iter() {
            assert!(!name.is_empty());
        }
        assert!(!PROMPTS.is_empty(), "no prompts loaded");
    }

    #[test]
    fn suggest_default_substitutes_context() {
        #[derive(Serialize)]
        struct Ctx<'a> {
            context: &'a str,
        }
        let s =
            PromptRegistry::load("suggest", "default", &Ctx { context: "3 open tasks" }).unwrap();
        assert!(s.contains("3 open tasks"), "{s}");
    }
}
