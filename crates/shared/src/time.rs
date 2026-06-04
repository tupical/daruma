use chrono::{DateTime, Utc};

/// Canonical wall-clock timestamp used across the domain (UTC).
pub type Timestamp = DateTime<Utc>;

/// Returns the current UTC wall-clock time.
#[inline]
pub fn now() -> Timestamp {
    Utc::now()
}
