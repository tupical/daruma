//! Task complexity hint — output of the §3.8.3 batch analysis pipeline.
//!
//! These types are pure data (no I/O). The AI crate produces a
//! `Vec<ComplexityHint>` from a `Vec<TaskBrief>` in one LLM call;
//! the storage crate then upserts the hints into the
//! `task_complexity_hints` projection.
//!
//! Complexity is *not* a `Task` field — see ROADMAP §3.8 ("complexity
//! score как Task field" is in the "what we don't take from CTM" list).

use daruma_shared::{TaskId, Timestamp};
use serde::{Deserialize, Serialize};

/// Minimal task context handed to the analyser (title + optional description).
///
/// Kept narrow on purpose: the LLM only needs enough to gauge fan-out,
/// and a smaller payload keeps the batch prompt within token limits.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TaskBrief {
    pub task_id: TaskId,
    pub title: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
}

/// One row in the `task_complexity_hints` projection.
///
/// `score` is constrained to 1..=10 (clamped at parse time). A higher
/// score means the task warrants a larger decomposition; `expansion_hint`
/// is a short imperative string the §3.8.4 hint-aware decompose tool
/// will feed back into the prompt.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ComplexityHint {
    pub task_id: TaskId,
    pub score: u8,
    pub recommended_subtasks: u8,
    pub expansion_hint: String,
    pub reasoning: String,
    pub generated_at: Timestamp,
    pub batch_id: String,
}
