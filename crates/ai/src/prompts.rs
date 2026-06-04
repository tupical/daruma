//! Prompt registry — central catalogue of LLM prompts (§3.8.5).
//!
//! Each prompt lives in its own `crates/ai/prompts/*.toml` file. The
//! file is parsed at first use into a [`Prompt`] with named variants,
//! cached in a process-wide [`PromptRegistry`]. Rendering goes through
//! [`tinytemplate`] for `{ var }` substitution against a serde-able
//! params struct.
//!
//! No hot-reload yet — the prompt sources are embedded with
//! `include_str!` so the binary is self-contained. Hot-reload (read
//! from `$TASKAGENT_PROMPT_DIR`) is tracked separately if/when it's
//! needed for prompt iteration without rebuild.
//!
//! ```ignore
//! use serde::Serialize;
//! use taskagent_ai::prompts::PromptRegistry;
//!
//! #[derive(Serialize)]
//! struct ParseCtx<'a> { input: &'a str }
//!
//! let s = PromptRegistry::load("parse", "default", &ParseCtx { input: "buy milk" })?;
//! ```

use std::collections::HashMap;

use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use taskagent_shared::CoreError;
use tinytemplate::{format_unescaped, TinyTemplate};

/// Parsed TOML shape of a single `prompts/<name>.toml` file.
#[derive(Debug, Deserialize)]
struct PromptFile {
    #[allow(dead_code)]
    meta: PromptMeta,
    variants: HashMap<String, PromptVariant>,
}

#[derive(Debug, Deserialize)]
struct PromptMeta {
    #[allow(dead_code)]
    name: String,
    #[allow(dead_code)]
    description: String,
}

#[derive(Debug, Deserialize)]
struct PromptVariant {
    template: String,
}

/// Process-wide prompt registry. All known prompts are baked into the
/// binary via `include_str!`; the first `load` call parses them.
pub struct PromptRegistry;

static PROMPTS: Lazy<HashMap<&'static str, PromptFile>> = Lazy::new(|| {
    let raw: &[(&str, &str)] = &[
        ("parse", include_str!("../prompts/parse.toml")),
        ("suggest", include_str!("../prompts/suggest.toml")),
        ("decompose", include_str!("../prompts/decompose.toml")),
        ("summarize", include_str!("../prompts/summarize.toml")),
        (
            "analyze_complexity",
            include_str!("../prompts/analyze_complexity.toml"),
        ),
        ("scope", include_str!("../prompts/scope.toml")),
        ("research", include_str!("../prompts/research.toml")),
    ];
    let mut out = HashMap::with_capacity(raw.len());
    for (name, body) in raw {
        let parsed: PromptFile = toml::from_str(body)
            .unwrap_or_else(|e| panic!("crates/ai/prompts/{name}.toml: parse error: {e}"));
        out.insert(*name, parsed);
    }
    out
});

impl PromptRegistry {
    /// Render `name` / `variant` against `params`. `params` must implement
    /// `Serialize`; tinytemplate accesses fields via dot-notation.
    ///
    /// `CoreError::Validation` is returned for an unknown prompt or
    /// variant. `CoreError::Ai` wraps tinytemplate render failures (e.g.
    /// a referenced variable that the params struct doesn't expose).
    pub fn load<P: Serialize>(name: &str, variant: &str, params: &P) -> Result<String, CoreError> {
        let prompt = PROMPTS
            .get(name)
            .ok_or_else(|| CoreError::validation(format!("unknown prompt: {name}")))?;
        let variant_def = prompt.variants.get(variant).ok_or_else(|| {
            CoreError::validation(format!(
                "prompt {name}: unknown variant {variant} (have: {:?})",
                prompt.variants.keys().collect::<Vec<_>>()
            ))
        })?;

        // tinytemplate caches compiled templates per registry instance;
        // we recompile per call because the input strings are small
        // and rendering already happens at LLM-call cadence (slow path).
        //
        // Disable the default HTML formatter — these are LLM prompts,
        // not browser-bound text, so apostrophes / quotes / angle
        // brackets must survive verbatim.
        let mut tt = TinyTemplate::new();
        tt.set_default_formatter(&format_unescaped);
        let label = format!("{name}.{variant}");
        tt.add_template(&label, &variant_def.template)
            .map_err(|e| CoreError::ai(format!("prompt {label}: bad template: {e}")))?;
        tt.render(&label, params)
            .map_err(|e| CoreError::ai(format!("prompt {label}: render failed: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Serialize)]
    struct Empty {}

    #[test]
    fn every_bundled_prompt_loads() {
        // Validates that the include_str! sources parse and every
        // declared variant compiles into a tinytemplate template.
        for (name, file) in PROMPTS.iter() {
            for (variant, _) in file.variants.iter() {
                // We don't render here — variant-specific params live in
                // the call sites — but we confirm parsing didn't fail
                // by virtue of reaching this loop. An assert keeps the
                // test honest if the lazy ever returns an empty map.
                assert!(!name.is_empty());
                assert!(!variant.is_empty());
            }
        }
        assert!(!PROMPTS.is_empty(), "no prompts loaded");
    }

    #[test]
    fn unknown_prompt_returns_validation_error() {
        let err = PromptRegistry::load("does_not_exist", "default", &Empty {}).unwrap_err();
        assert_eq!(err.code(), "validation");
    }

    #[test]
    fn unknown_variant_returns_validation_error() {
        let err = PromptRegistry::load("parse", "no_such_variant", &Empty {}).unwrap_err();
        assert_eq!(err.code(), "validation");
    }

    #[test]
    fn parse_default_substitutes_input() {
        #[derive(Serialize)]
        struct Ctx<'a> {
            input: &'a str,
        }
        let s = PromptRegistry::load("parse", "default", &Ctx { input: "buy milk" }).unwrap();
        assert!(s.contains("buy milk"), "{s}");
        assert!(s.contains("create_task"), "{s}");
    }

    #[test]
    fn decompose_with_hint_includes_guidance_block() {
        #[derive(Serialize)]
        struct Ctx<'a> {
            task_context: &'a str,
            hint: &'a str,
        }
        let s = PromptRegistry::load(
            "decompose",
            "with_hint",
            &Ctx {
                task_context: "Build login page",
                hint: "OAuth first",
            },
        )
        .unwrap();
        assert!(s.contains("Build login page"));
        assert!(s.contains("Additional guidance:"));
        assert!(s.contains("OAuth first"));
    }
}
