//! Pure domain entities. No I/O, no async.

pub mod activity;
pub mod agent;
pub mod comment;
pub mod complexity;
pub mod device;
pub mod document;
pub mod external_ref;
pub mod plan;
pub mod project;
pub mod relation;
pub mod run;
pub mod session;
pub mod signal;
pub mod task;

pub use activity::{Activity, Verb};
pub use agent::{Actor, AgentAction, AgentActionKind};
pub use comment::{Comment, CommentKind, CommentPatch, NewComment};
pub use complexity::{ComplexityHint, TaskBrief};
pub use device::Device;
pub use document::{Document, DocumentKind, NewDocument};
pub use external_ref::ExternalRef;
pub use plan::{
    CanStart, CanStartBlocker, NewPlan, Plan, PlanFanoutWave, PlanGraph, PlanGraphEdge,
    PlanGraphNode, PlanPatch, PlanProgress, PlanProgressSummary, PlanStatus, PlanTask,
};
pub use project::{slugify_title, Project, DEFAULT_TENANT_ID};
pub use relation::{Relation, RelationKind, TaskRelations};
pub use run::{Run, RunNote, RunOutcome, RunStatus};
pub use session::{
    AgentSession, AgentSessionPlanStep, SessionArtifact, SessionArtifactKind, SessionStepStatus,
};
pub use signal::SignalKind;
pub use task::{NewTask, Priority, Status, Task, TaskPatch, TriageState};
