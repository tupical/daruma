//! Typed run signals — Linear B.5: structured bidirectional signal bus between
//! human and agent during an active Run.

use serde::{Deserialize, Serialize};

/// Typed signal exchanged on the `Channel::Runs` bus.
///
/// Uses `tag = "kind"` so every variant carries an explicit discriminant field
/// in JSON, consistent with the rest of the domain's tagged-enum convention.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SignalKind {
    /// Request the agent to stop the current run.
    Stop { reason: Option<String> },
    /// Ask the agent (or human) to pick one of the offered choices.
    Elicit {
        prompt: String,
        choices: Vec<String>,
    },
    /// Agent requires external auth before proceeding.
    AuthRequired { scope: String },
    /// Human accepted an elicitation choice.
    InterventionAccepted { choice: String },
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signal_stop_roundtrip_serde() {
        for signal in [
            SignalKind::Stop { reason: None },
            SignalKind::Stop {
                reason: Some("user cancelled".to_string()),
            },
        ] {
            let json = serde_json::to_string(&signal).unwrap();
            let back: SignalKind = serde_json::from_str(&json).unwrap();
            assert_eq!(signal, back, "roundtrip failed for {signal:?}");
        }
    }

    #[test]
    fn signal_elicit_roundtrip_serde() {
        let signal = SignalKind::Elicit {
            prompt: "Which environment?".to_string(),
            choices: vec!["prod".to_string(), "staging".to_string()],
        };
        let json = serde_json::to_string(&signal).unwrap();
        let back: SignalKind = serde_json::from_str(&json).unwrap();
        assert_eq!(signal, back);
    }

    #[test]
    fn signal_auth_required_roundtrip_serde() {
        let signal = SignalKind::AuthRequired {
            scope: "github:write".to_string(),
        };
        let json = serde_json::to_string(&signal).unwrap();
        let back: SignalKind = serde_json::from_str(&json).unwrap();
        assert_eq!(signal, back);
    }

    #[test]
    fn signal_intervention_accepted_roundtrip_serde() {
        let signal = SignalKind::InterventionAccepted {
            choice: "prod".to_string(),
        };
        let json = serde_json::to_string(&signal).unwrap();
        let back: SignalKind = serde_json::from_str(&json).unwrap();
        assert_eq!(signal, back);
    }

    #[test]
    fn signal_has_kind_tag_field() {
        let signal = SignalKind::Stop { reason: None };
        let json = serde_json::to_string(&signal).unwrap();
        assert!(
            json.contains("\"kind\""),
            "expected 'kind' discriminant, got: {json}"
        );
        assert!(
            json.contains("\"stop\""),
            "expected 'stop' value, got: {json}"
        );
    }

    #[test]
    fn all_variants_roundtrip_serde() {
        let variants = vec![
            SignalKind::Stop { reason: None },
            SignalKind::Elicit {
                prompt: "p".to_string(),
                choices: vec!["a".to_string()],
            },
            SignalKind::AuthRequired {
                scope: "s".to_string(),
            },
            SignalKind::InterventionAccepted {
                choice: "c".to_string(),
            },
        ];
        for signal in variants {
            let json = serde_json::to_string(&signal).unwrap();
            let back: SignalKind = serde_json::from_str(&json).unwrap();
            assert_eq!(signal, back, "roundtrip failed for {signal:?}");
        }
    }
}
