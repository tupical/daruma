use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Generate strongly-typed UUIDv7 newtypes with a human-readable prefix.
macro_rules! newtype_id {
    ($name:ident, $prefix:literal) => {
        #[derive(
            Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(pub Uuid);

        impl $name {
            #[inline]
            pub fn new() -> Self {
                Self(Uuid::now_v7())
            }

            #[inline]
            pub fn from_uuid(uuid: Uuid) -> Self {
                Self(uuid)
            }

            #[inline]
            pub fn as_uuid(&self) -> Uuid {
                self.0
            }

            #[inline]
            pub const fn prefix() -> &'static str {
                $prefix
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}_{}", $prefix, self.0)
            }
        }

        impl FromStr for $name {
            type Err = uuid::Error;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                let trimmed = s.strip_prefix(concat!($prefix, "_")).unwrap_or(s);
                Ok(Self(Uuid::parse_str(trimmed)?))
            }
        }
    };
}

newtype_id!(TaskId, "tsk");
newtype_id!(AiOpId, "aiop");
newtype_id!(WorkUnitId, "wu");
newtype_id!(ProjectId, "prj");
newtype_id!(EventId, "evt");
newtype_id!(AgentId, "agt");
newtype_id!(DeviceId, "dev");
newtype_id!(ActivityId, "act");
newtype_id!(CommentId, "cmt");
newtype_id!(TokenId, "tok");
newtype_id!(WebhookId, "wh");
newtype_id!(WebhookDeliveryId, "whd");
newtype_id!(PlanId, "pln");
newtype_id!(RunId, "run");
newtype_id!(RunNoteId, "rnt");
newtype_id!(AgentSessionId, "ags");
newtype_id!(SessionArtifactId, "saf");
newtype_id!(RelationId, "rel");
newtype_id!(DocumentId, "doc");
newtype_id!(VersionId, "ver");
newtype_id!(WorkLeaseId, "wls");
newtype_id!(ArtifactId, "art");
newtype_id!(ArtifactRelationId, "artrel");
newtype_id!(RuleId, "rule");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique() {
        let a = TaskId::new();
        let b = TaskId::new();
        assert_ne!(a, b);
    }

    #[test]
    fn display_includes_prefix() {
        let id = TaskId::new();
        let s = id.to_string();
        assert!(s.starts_with("tsk_"), "got: {s}");
    }

    #[test]
    fn roundtrip_display_parse() {
        let id = ProjectId::new();
        let parsed: ProjectId = id.to_string().parse().unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn parse_accepts_bare_uuid_too() {
        let id = EventId::new();
        let bare = id.as_uuid().to_string();
        let parsed: EventId = bare.parse().unwrap();
        assert_eq!(id, parsed);
    }
}
