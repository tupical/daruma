//! Prompt rendering engine (§3.8.5) — the provider-neutral mechanism.
//!
//! This module holds only the *machinery*: parsing a `prompts/<name>.toml`
//! file into named variants and rendering one through [`tinytemplate`] for
//! `{ var }` substitution against a serde-able params struct.
//!
//! The *catalogue* of operation prompts (parse, decompose, scope, …) lives
//! with the operations in `taskagent-ai`; those prompts are operational, not
//! infrastructure. A crate owning a set of prompts embeds its `*.toml` via
//! `include_str!`, parses each with [`PromptFile::parse`], and renders a
//! chosen variant with [`render_variant`].
//!
//! ```ignore
//! use serde::Serialize;
//! use taskagent_ai_infra::prompts::{PromptFile, render_variant};
//!
//! #[derive(Serialize)]
//! struct ParseCtx<'a> { input: &'a str }
//!
//! let file = PromptFile::parse("parse", include_str!("../prompts/parse.toml"))?;
//! let s = render_variant("parse", "default", &file, &ParseCtx { input: "buy milk" })?;
//! ```

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use taskagent_shared::CoreError;
use tinytemplate::{format_unescaped, TinyTemplate};

/// Parsed TOML shape of a single `prompts/<name>.toml` file.
#[derive(Debug, Deserialize)]
pub struct PromptFile {
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

impl PromptFile {
    /// Parse the raw TOML body of a `prompts/<name>.toml` file. `name` is
    /// only used to enrich the error message. Panics-free; returns
    /// [`CoreError::Validation`] on malformed TOML so callers can decide
    /// whether to `expect` (bundled, build-time-correct sources) or
    /// surface the error.
    pub fn parse(name: &str, body: &str) -> Result<Self, CoreError> {
        toml::from_str(body)
            .map_err(|e| CoreError::validation(format!("prompt {name}: parse error: {e}")))
    }

    /// Names of the variants declared in this file. Used by callers for
    /// error messages.
    pub fn variant_names(&self) -> Vec<&str> {
        self.variants.keys().map(String::as_str).collect()
    }

    /// Borrow a variant's raw template by name, if present.
    pub fn template(&self, variant: &str) -> Option<&str> {
        self.variants.get(variant).map(|v| v.template.as_str())
    }
}

/// Render `variant` of an already-parsed `file` against `params`.
///
/// `name` is used only to label errors. `CoreError::Validation` is returned
/// for an unknown variant; `CoreError::Ai` wraps tinytemplate render
/// failures (e.g. a referenced variable the params struct doesn't expose).
pub fn render_variant<P: Serialize>(
    name: &str,
    variant: &str,
    file: &PromptFile,
    params: &P,
) -> Result<String, CoreError> {
    let template = file.template(variant).ok_or_else(|| {
        CoreError::validation(format!(
            "prompt {name}: unknown variant {variant} (have: {:?})",
            file.variant_names()
        ))
    })?;

    // tinytemplate caches compiled templates per registry instance; we
    // recompile per call because the input strings are small and rendering
    // already happens at LLM-call cadence (slow path).
    //
    // Disable the default HTML formatter — these are LLM prompts, not
    // browser-bound text, so apostrophes / quotes / angle brackets must
    // survive verbatim.
    let mut tt = TinyTemplate::new();
    tt.set_default_formatter(&format_unescaped);
    let label = format!("{name}.{variant}");
    tt.add_template(&label, template)
        .map_err(|e| CoreError::ai(format!("prompt {label}: bad template: {e}")))?;
    tt.render(&label, params)
        .map_err(|e| CoreError::ai(format!("prompt {label}: render failed: {e}")))
}

/// A parsed catalogue of `prompts/<name>.toml` files, keyed by prompt name.
///
/// Each crate that owns a set of operation prompts embeds its `*.toml` via
/// `include_str!` and builds one process-wide registry in a `Lazy`:
///
/// ```ignore
/// use once_cell::sync::Lazy;
/// use taskagent_ai_infra::prompts::PromptRegistry;
///
/// static PROMPTS: Lazy<PromptRegistry> = Lazy::new(|| {
///     PromptRegistry::new(&[("parse", include_str!("../prompts/parse.toml"))])
/// });
/// ```
pub struct PromptRegistry(HashMap<&'static str, PromptFile>);

impl PromptRegistry {
    /// Parse every `(name, toml_body)` pair into the catalogue. Sources are
    /// bundled at build time, so a parse failure or a duplicate name is a
    /// programmer error: this panics with the offending name rather than
    /// returning a `Result` no caller could recover from.
    pub fn new(raw: &[(&'static str, &'static str)]) -> Self {
        let mut out = HashMap::with_capacity(raw.len());
        for (name, body) in raw {
            let parsed = PromptFile::parse(name, body)
                .unwrap_or_else(|e| panic!("prompts/{name}.toml: {e}"));
            if out.insert(*name, parsed).is_some() {
                panic!("prompts: duplicate prompt name {name}");
            }
        }
        Self(out)
    }

    /// Render `name` / `variant` against `params`. `params` must implement
    /// `Serialize`; tinytemplate accesses fields via dot-notation.
    ///
    /// `CoreError::Validation` is returned for an unknown prompt or
    /// variant. `CoreError::Ai` wraps tinytemplate render failures (e.g.
    /// a referenced variable that the params struct doesn't expose).
    pub fn load<P: Serialize>(
        &self,
        name: &str,
        variant: &str,
        params: &P,
    ) -> Result<String, CoreError> {
        let prompt = self
            .0
            .get(name)
            .ok_or_else(|| CoreError::validation(format!("unknown prompt: {name}")))?;
        render_variant(name, variant, prompt, params)
    }

    /// Iterate over `(name, file)` pairs. Used by crate smoke tests that want
    /// to assert their bundled sources all parsed.
    pub fn iter(&self) -> impl Iterator<Item = (&'static str, &PromptFile)> {
        self.0.iter().map(|(k, v)| (*k, v))
    }

    /// Number of prompts in the catalogue.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the catalogue is empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
[meta]
name = "sample"
description = "for tests"

[variants.default]
template = "hello { name }"
"#;

    #[derive(Serialize)]
    struct Ctx<'a> {
        name: &'a str,
    }

    #[test]
    fn parse_and_render_default() {
        let file = PromptFile::parse("sample", SAMPLE).unwrap();
        let s = render_variant("sample", "default", &file, &Ctx { name: "world" }).unwrap();
        assert_eq!(s, "hello world");
    }

    #[test]
    fn unknown_variant_returns_validation_error() {
        let file = PromptFile::parse("sample", SAMPLE).unwrap();
        let err = render_variant("sample", "missing", &file, &Ctx { name: "x" }).unwrap_err();
        assert_eq!(err.code(), "validation");
    }

    #[test]
    fn malformed_toml_returns_validation_error() {
        let err = PromptFile::parse("bad", "this is not = valid toml [[[").unwrap_err();
        assert_eq!(err.code(), "validation");
    }

    #[test]
    fn registry_loads_and_renders() {
        let reg = PromptRegistry::new(&[("sample", SAMPLE)]);
        assert_eq!(reg.len(), 1);
        assert!(!reg.is_empty());
        let s = reg
            .load("sample", "default", &Ctx { name: "world" })
            .unwrap();
        assert_eq!(s, "hello world");
    }

    #[test]
    fn registry_unknown_prompt_returns_validation_error() {
        let reg = PromptRegistry::new(&[("sample", SAMPLE)]);
        let err = reg
            .load("does_not_exist", "default", &Ctx { name: "x" })
            .unwrap_err();
        assert_eq!(err.code(), "validation");
    }

    #[test]
    fn registry_unknown_variant_returns_validation_error() {
        let reg = PromptRegistry::new(&[("sample", SAMPLE)]);
        let err = reg
            .load("sample", "missing", &Ctx { name: "x" })
            .unwrap_err();
        assert_eq!(err.code(), "validation");
    }

    #[test]
    #[should_panic(expected = "duplicate prompt name")]
    fn registry_rejects_duplicate_names() {
        let _ = PromptRegistry::new(&[("sample", SAMPLE), ("sample", SAMPLE)]);
    }

    #[test]
    #[should_panic(expected = "prompts/bad.toml")]
    fn registry_panics_on_malformed_source() {
        let _ = PromptRegistry::new(&[("bad", "not = valid [[[")]);
    }
}
