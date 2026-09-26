//! Plan source chain (ADR-0009 «цепочка источников»): a source is a node
//! addressed by a URI, pointing at the source above it (`upstream_ref`).
//! `plans.source_ref` names the nearest node.

use daruma_shared::Timestamp;
use serde::{Deserialize, Serialize};

use crate::agent::Actor;
use crate::plan::normalize_source_ref;

/// Most nodes a chain may hold, and the bound of every traversal.
pub const SOURCE_CHAIN_MAX: usize = 32;
const LABEL_MAX_LEN: usize = 512;
const NOTE_MAX_LEN: usize = 4096;

/// A stored source node, as carried by `SourceUpserted`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SourceNode {
    #[serde(rename = "ref")]
    pub source_ref: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub occurred_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_ref: Option<String>,
    pub created_at: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<Actor>,
}

impl SourceNode {
    /// Fill-only merge: empty fields take the incoming value, set ones stay.
    /// `Err(existing upstream)` when both carry a different `upstream_ref`.
    pub fn fill_from(&self, incoming: &SourceNode) -> Result<SourceNode, String> {
        if let (Some(have), Some(new)) = (&self.upstream_ref, &incoming.upstream_ref) {
            if have != new {
                return Err(have.clone());
            }
        }
        let fill = |a: &Option<String>, b: &Option<String>| a.clone().or_else(|| b.clone());
        Ok(SourceNode {
            label: fill(&self.label, &incoming.label),
            occurred_at: fill(&self.occurred_at, &incoming.occurred_at),
            note: fill(&self.note, &incoming.note),
            upstream_ref: fill(&self.upstream_ref, &incoming.upstream_ref),
            ..self.clone()
        })
    }
}

/// A node as a client sends it (intake `plan.source`, `ExtendSource`).
/// `upstream` is ordered nearest → farthest and only allowed on the top
/// level.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceInput {
    #[serde(rename = "ref", default, skip_serializing_if = "Option::is_none")]
    pub source_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub occurred_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub upstream: Vec<SourceInput>,
}

fn trimmed(value: &Option<String>, what: &str, max: usize) -> Result<Option<String>, String> {
    let Some(value) = value.as_deref().map(str::trim).filter(|v| !v.is_empty()) else {
        return Ok(None);
    };
    if value.chars().count() > max {
        return Err(format!("source {what} is longer than {max} characters"));
    }
    Ok(Some(value.to_string()))
}

impl SourceInput {
    /// Whether the client named a node at all (a ref or a label).
    pub fn names_node(&self) -> bool {
        self.source_ref.is_some() || self.label.is_some()
    }

    /// Validate and normalise one node (not its `upstream`). A node without
    /// `ref` gets `note:<synthetic>` from `synthetic_ref`; one with neither
    /// `ref` nor `label` is rejected. `occurred_at` is RFC 3339 or
    /// `YYYY-MM-DD`, stored as sent.
    pub fn to_node(
        &self,
        synthetic_ref: impl FnOnce() -> String,
        created_at: Timestamp,
        created_by: &Actor,
    ) -> Result<SourceNode, String> {
        let label = trimmed(&self.label, "label", LABEL_MAX_LEN)?;
        let source_ref = match (&self.source_ref, &label) {
            (Some(raw), _) => normalize_source_ref(raw)?,
            (None, Some(_)) => synthetic_ref(),
            (None, None) => return Err("a source node needs `ref` or `label`".into()),
        };
        let occurred_at = trimmed(&self.occurred_at, "occurred_at", 64)?;
        if let Some(at) = &occurred_at {
            let valid = chrono::DateTime::parse_from_rfc3339(at).is_ok()
                || chrono::NaiveDate::parse_from_str(at, "%Y-%m-%d").is_ok();
            if !valid {
                return Err(format!(
                    "source occurred_at `{at}` must be RFC 3339 or YYYY-MM-DD"
                ));
            }
        }
        Ok(SourceNode {
            source_ref,
            label,
            occurred_at,
            note: trimmed(&self.note, "note", NOTE_MAX_LEN)?,
            upstream_ref: None,
            created_at,
            created_by: Some(created_by.clone()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use daruma_shared::time;

    fn node(input: SourceInput) -> Result<SourceNode, String> {
        input.to_node(|| "note:x".into(), time::now(), &Actor::user())
    }

    #[test]
    fn to_node_validates_and_synthesises() {
        let n = node(SourceInput {
            label: Some(" Call ".into()),
            occurred_at: Some("2026-01-01".into()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(
            (n.source_ref.as_str(), n.label.as_deref()),
            ("note:x", Some("Call"))
        );
        let n = node(SourceInput {
            source_ref: Some("HTTPS://X.io/a/".into()),
            occurred_at: Some("2026-01-01T18:30:00+03:00".into()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(n.source_ref, "https://x.io/a");
        assert!(node(SourceInput::default()).is_err());
        assert!(node(SourceInput {
            label: Some("x".into()),
            occurred_at: Some("01.01.2026".into()),
            ..Default::default()
        })
        .is_err());
    }

    #[test]
    fn fill_from_is_fill_only() {
        let base = node(SourceInput {
            source_ref: Some("mailto:a@x".into()),
            label: Some("old".into()),
            ..Default::default()
        })
        .unwrap();
        let mut incoming = base.clone();
        incoming.label = Some("new".into());
        incoming.note = Some("n".into());
        incoming.upstream_ref = Some("note:1".into());
        let merged = base.fill_from(&incoming).unwrap();
        assert_eq!(merged.label.as_deref(), Some("old"));
        assert_eq!(merged.note.as_deref(), Some("n"));
        assert_eq!(merged.upstream_ref.as_deref(), Some("note:1"));
        incoming.upstream_ref = Some("note:2".into());
        assert_eq!(merged.fill_from(&incoming), Err("note:1".into()));
    }
}
