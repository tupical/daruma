//! Per-project settings — currently the auto-append toggles for the
//! project's narrative `Interview` (AI log) and `Human Log` documents.
//! These docs are no longer auto-created by the core; when a narrative
//! document of the matching kind exists, activity is appended to it.

use serde::{Deserialize, Serialize};

/// Auto-append toggles. Both logs are **enabled by default** — including
/// for projects created before the setting existed (no stored row =
/// defaults).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutoAppendSettings {
    /// Append agent-driven activity (agent task ops, runs, run notes) to
    /// the project's `Interview` document.
    pub interview: bool,
    /// Append human-readable milestones (user task ops, plan completion,
    /// project renames) to the project's `Human Log` document.
    pub human_log: bool,
}

impl Default for AutoAppendSettings {
    fn default() -> Self {
        Self {
            interview: true,
            human_log: true,
        }
    }
}

impl AutoAppendSettings {
    pub fn apply(mut self, patch: AutoAppendPatch) -> Self {
        if let Some(v) = patch.interview {
            self.interview = v;
        }
        if let Some(v) = patch.human_log {
            self.human_log = v;
        }
        self
    }
}

/// Partial update for [`AutoAppendSettings`]; `None` leaves a flag as-is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutoAppendPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interview: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub human_log: Option<bool>,
}

/// Project policy for plan sources (ADR-0009), stored under the
/// `intake_source` settings key. No stored key behaves as
/// `IntakeSourcePolicy::default()`: `warn` with no channels.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IntakeSourcePolicy {
    pub mode: IntakeSourceMode,
    /// Fallback ref when nothing else resolved, used literally (the cabinet
    /// writes a concrete ref, e.g. `self://alice` for a solo project).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub derive: Vec<SourceDeriveRule>,
    /// Allowed channels; empty = any scheme.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub channels: Vec<SourceChannel>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntakeSourceMode {
    /// Resolve the ref, never check it.
    Off,
    /// Create the plan, answer with a warning.
    #[default]
    Warn,
    /// Reject the plan with `plan_source_required`.
    Enforce,
}

/// Derive a ref from the work context: `pattern` (regex) is matched against
/// the context value named by `from` (only `branch` today) and its groups
/// `$1..$9` are substituted into `ref`; a rule whose template names a group
/// that did not participate in the match does not fire.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceDeriveRule {
    pub from: String,
    pub pattern: String,
    #[serde(rename = "ref")]
    pub ref_template: String,
}

/// An allowed source channel: a URI scheme, optionally narrowed by a regex
/// over the whole normalised ref.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceChannel {
    pub scheme: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// The channel carries no content by reference, so the plan must carry
    /// a `source_brief` note.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub note_required: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_on_and_patch_merges() {
        let s = AutoAppendSettings::default();
        assert!(s.interview && s.human_log);
        let s = s.apply(AutoAppendPatch {
            interview: Some(false),
            human_log: None,
        });
        assert!(!s.interview);
        assert!(s.human_log, "unset patch field leaves the flag unchanged");
    }
}
