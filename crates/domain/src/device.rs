//! Device identity for multi-device event origins.

use serde::{Deserialize, Serialize};
use daruma_shared::{DeviceId, Timestamp};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Device {
    pub id: DeviceId,
    pub label: String,
    pub created_at: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen_at: Option<Timestamp>,
}
