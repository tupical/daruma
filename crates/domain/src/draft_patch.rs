//! Draft patch with context anchors (ADR-0011 п.6).
//!
//! A draft patch is the payload of a `role=draft kind=patch` role artifact.
//! It never addresses code by line number: every edit names the enclosing
//! symbol and carries the unchanged lines around the change. The builder
//! revalidates the anchor against the file as it is *now*; an exact,
//! unique match is applied, anything else is a mismatch that asks for a
//! regenerated draft. There is no fuzzy or approximate application.

use serde::{Deserialize, Serialize};

/// Role artifact kind that carries a [`DraftPatch`] payload.
pub const DRAFT_PATCH_KIND: &str = "patch";

/// Payload of a draft patch. Unknown fields (e.g. a `line` number) are
/// rejected, so a line-addressed patch cannot slip through.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DraftPatch {
    /// Repository-relative file path; authoritative — the patch applies to
    /// this file only.
    pub path: String,
    /// Edits applied in order; all or nothing.
    pub edits: Vec<AnchoredEdit>,
}

/// One change, located by its anchor: `symbol` plus the exact unchanged
/// lines `before` and `after` the replaced lines `old`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnchoredEdit {
    /// Name of the enclosing symbol/function, e.g. `fn hire_employee`. It
    /// must occur in the file at or before the end of the anchored window.
    pub symbol: String,
    #[serde(default)]
    pub before: Vec<String>,
    /// Lines being replaced; empty for a pure insertion.
    #[serde(default)]
    pub old: Vec<String>,
    /// Replacement lines; empty for a pure deletion.
    #[serde(default)]
    pub new: Vec<String>,
    #[serde(default)]
    pub after: Vec<String>,
}

/// Why an anchor did not match. Every variant means "do not apply, request a
/// regenerated draft"; the file stays untouched.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum AnchorMismatch {
    /// The payload is not a valid draft patch (unknown field, empty anchor, …).
    InvalidPatch { message: String },
    /// The caller's file is not the one the patch targets.
    PathMismatch { expected: String, actual: String },
    /// `before + old + after` does not occur in the file.
    ContextNotFound { edit: usize },
    /// The anchored window occurs more than once; picking one would be a guess.
    Ambiguous { edit: usize, matches: usize },
    /// The window matched, but `symbol` does not occur at or before it.
    SymbolNotFound { edit: usize, symbol: String },
}

impl AnchorMismatch {
    /// Stable snake_case code, e.g. `context_not_found`.
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidPatch { .. } => "invalid_patch",
            Self::PathMismatch { .. } => "path_mismatch",
            Self::ContextNotFound { .. } => "context_not_found",
            Self::Ambiguous { .. } => "ambiguous",
            Self::SymbolNotFound { .. } => "symbol_not_found",
        }
    }
}

impl DraftPatch {
    /// Parse a role artifact payload; any shape error is an
    /// [`AnchorMismatch::InvalidPatch`].
    pub fn from_payload(payload: &serde_json::Value) -> Result<Self, AnchorMismatch> {
        let patch: Self =
            serde_json::from_value(payload.clone()).map_err(|e| AnchorMismatch::InvalidPatch {
                message: e.to_string(),
            })?;
        patch.validate()?;
        Ok(patch)
    }

    /// Structural checks: a path, at least one edit, every edit anchored by
    /// a symbol and at least one context line, lines without newlines.
    pub fn validate(&self) -> Result<(), AnchorMismatch> {
        let invalid = |message: String| Err(AnchorMismatch::InvalidPatch { message });
        if self.path.trim().is_empty() {
            return invalid("path must not be empty".into());
        }
        if self.edits.is_empty() {
            return invalid("edits must not be empty".into());
        }
        for (i, edit) in self.edits.iter().enumerate() {
            if edit.symbol.trim().is_empty() {
                return invalid(format!("edit {i}: symbol must not be empty"));
            }
            if edit.before.is_empty() && edit.after.is_empty() {
                return invalid(format!("edit {i}: needs context lines before or after"));
            }
            if edit.old == edit.new {
                return invalid(format!("edit {i}: old and new are identical"));
            }
            let lines = [&edit.before, &edit.old, &edit.new, &edit.after];
            if lines.iter().any(|ls| ls.iter().any(|l| l.contains('\n'))) {
                return invalid(format!("edit {i}: a line must not contain a newline"));
            }
        }
        Ok(())
    }

    /// Revalidate every anchor against `text` (the current content of
    /// `path`) and return the patched text. All edits apply or none: on any
    /// mismatch the caller gets the reason and keeps its file unchanged.
    pub fn apply(&self, path: &str, text: &str) -> Result<String, AnchorMismatch> {
        self.validate()?;
        if path != self.path {
            return Err(AnchorMismatch::PathMismatch {
                expected: self.path.clone(),
                actual: path.to_string(),
            });
        }
        let mut lines: Vec<String> = text.split('\n').map(str::to_owned).collect();
        for (i, edit) in self.edits.iter().enumerate() {
            lines = edit.apply_to(i, lines)?;
        }
        Ok(lines.join("\n"))
    }
}

impl AnchoredEdit {
    fn apply_to(&self, index: usize, lines: Vec<String>) -> Result<Vec<String>, AnchorMismatch> {
        let window: Vec<&String> = self
            .before
            .iter()
            .chain(&self.old)
            .chain(&self.after)
            .collect();
        let starts: Vec<usize> = (0..=lines.len().saturating_sub(window.len()))
            .filter(|&s| {
                s + window.len() <= lines.len()
                    && window.iter().zip(&lines[s..]).all(|(w, l)| *w == l)
            })
            .collect();
        let start = match starts.as_slice() {
            [] => return Err(AnchorMismatch::ContextNotFound { edit: index }),
            [one] => *one,
            many => {
                return Err(AnchorMismatch::Ambiguous {
                    edit: index,
                    matches: many.len(),
                })
            }
        };
        let end = start + window.len();
        if !lines[..end].iter().any(|l| l.contains(self.symbol.as_str())) {
            return Err(AnchorMismatch::SymbolNotFound {
                edit: index,
                symbol: self.symbol.clone(),
            });
        }
        let old_start = start + self.before.len();
        let old_end = old_start + self.old.len();
        let mut out = Vec::with_capacity(lines.len() + self.new.len());
        out.extend_from_slice(&lines[..old_start]);
        out.extend(self.new.iter().cloned());
        out.extend_from_slice(&lines[old_end..]);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const FILE: &str = "\
impl Department {
    fn hire_employee(&mut self, salary: i64) {
        self.headcount += 1;
        self.budget += salary;
    }
}
";

    fn hire_fix() -> DraftPatch {
        DraftPatch::from_payload(&json!({
            "path": "src/game/hiring.rs",
            "edits": [{
                "symbol": "fn hire_employee",
                "before": ["        self.headcount += 1;"],
                "old": ["        self.budget += salary;"],
                "new": ["        self.budget -= salary;"],
                "after": ["    }"]
            }]
        }))
        .unwrap()
    }

    #[test]
    fn applies_when_lines_shifted() {
        let shifted = format!("// header\n// added by someone else\n\n{FILE}");
        let out = hire_fix().apply("src/game/hiring.rs", &shifted).unwrap();
        assert_eq!(out, shifted.replace("budget += salary", "budget -= salary"));
        assert!(out.ends_with("}\n"), "trailing newline kept");
    }

    #[test]
    fn missing_context_is_a_mismatch_and_nothing_changes() {
        let changed = FILE.replace("headcount += 1", "headcount += 2");
        let err = hire_fix().apply("src/game/hiring.rs", &changed).unwrap_err();
        assert_eq!(err, AnchorMismatch::ContextNotFound { edit: 0 });
        assert_eq!(err.code(), "context_not_found");
    }

    #[test]
    fn whitespace_difference_is_not_fuzzily_matched() {
        let reindented = FILE.replace("        self.budget", "\tself.budget");
        assert!(matches!(
            hire_fix().apply("src/game/hiring.rs", &reindented),
            Err(AnchorMismatch::ContextNotFound { .. })
        ));
    }

    #[test]
    fn duplicate_window_is_ambiguous() {
        let twice = format!("{FILE}{FILE}");
        assert_eq!(
            hire_fix().apply("src/game/hiring.rs", &twice).unwrap_err(),
            AnchorMismatch::Ambiguous { edit: 0, matches: 2 }
        );
    }

    #[test]
    fn symbol_must_precede_the_window() {
        let renamed = FILE.replace("fn hire_employee", "fn recruit");
        assert_eq!(
            hire_fix().apply("src/game/hiring.rs", &renamed).unwrap_err(),
            AnchorMismatch::SymbolNotFound {
                edit: 0,
                symbol: "fn hire_employee".into()
            }
        );
    }

    #[test]
    fn path_is_authoritative() {
        assert!(matches!(
            hire_fix().apply("src/game/payroll.rs", FILE),
            Err(AnchorMismatch::PathMismatch { .. })
        ));
    }

    #[test]
    fn edits_are_all_or_nothing() {
        let mut patch = hire_fix();
        patch.edits.push(AnchoredEdit {
            symbol: "fn fire_employee".into(),
            before: vec!["    fn fire_employee(&mut self) {".into()],
            old: vec![],
            new: vec!["        self.headcount -= 1;".into()],
            after: vec![],
        });
        assert_eq!(
            patch.apply("src/game/hiring.rs", FILE).unwrap_err(),
            AnchorMismatch::ContextNotFound { edit: 1 }
        );
    }

    #[test]
    fn insertion_after_context() {
        let patch = DraftPatch::from_payload(&json!({
            "path": "a.rs",
            "edits": [{
                "symbol": "fn hire_employee",
                "before": ["        self.headcount += 1;"],
                "new": ["        self.log_hire();"]
            }]
        }))
        .unwrap();
        let out = patch.apply("a.rs", FILE).unwrap();
        assert!(out.contains("+= 1;\n        self.log_hire();\n        self.budget"));
    }

    #[test]
    fn line_numbers_are_not_part_of_the_format() {
        for extra in [json!({"line": 4}), json!({"line_number": 4})] {
            let mut edit = json!({
                "symbol": "fn hire_employee",
                "before": ["x"], "old": ["y"], "new": ["z"]
            });
            edit.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            let err = DraftPatch::from_payload(&json!({"path": "a.rs", "edits": [edit]}))
                .unwrap_err();
            assert_eq!(err.code(), "invalid_patch", "{err:?}");
        }
    }

    #[test]
    fn anchor_without_context_or_symbol_is_invalid() {
        for edit in [
            json!({"symbol": "fn x", "old": ["a"], "new": ["b"]}),
            json!({"symbol": " ", "before": ["a"], "new": ["b"]}),
            json!({"symbol": "fn x", "before": ["a"], "old": ["b"], "new": ["b"]}),
        ] {
            let err =
                DraftPatch::from_payload(&json!({"path": "a.rs", "edits": [edit]})).unwrap_err();
            assert_eq!(err.code(), "invalid_patch", "{err:?}");
        }
    }
}
