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
