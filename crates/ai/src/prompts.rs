//! Operation prompt catalogue.
//!
//! The prompt *rendering engine* and the shared [`SharedRegistry`] live in
//! `daruma-ai-infra`. This module only declares the catalogue of
//! operation prompts — one `crates/ai/prompts/*.toml` per operation
//! (analyze_complexity) — because those prompts are operational, not
//! infrastructure.
//!
//! All known prompts are baked into the binary via `include_str!`; the
//! first [`PromptRegistry::load`] call parses them.

use daruma_ai_infra::prompts::PromptRegistry as SharedRegistry;
use daruma_shared::CoreError;
use once_cell::sync::Lazy;
use serde::Serialize;

static PROMPTS: Lazy<SharedRegistry> = Lazy::new(|| {
    SharedRegistry::new(&[(
        "analyze_complexity",
        include_str!("../prompts/analyze_complexity.toml"),
    )])
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

    #[test]
    fn every_bundled_prompt_loads() {
        for (name, _file) in PROMPTS.iter() {
            assert!(!name.is_empty());
        }
        assert!(!PROMPTS.is_empty(), "no prompts loaded");
    }
}
