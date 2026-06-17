//! Pure domain entities. No I/O, no async.

pub mod activity;
pub mod agent;
pub mod artifact;
pub mod comment;
pub mod complexity;
pub mod device;
pub mod document;
pub mod evidence;
pub mod external_ref;
pub mod plan;
pub mod project;
pub mod project_settings;
pub mod relation;
pub mod rule;
pub mod run;
pub mod session;
pub mod signal;
pub mod task;
pub mod work_lease;
pub mod work_unit;

pub use activity::{Activity, Verb};
pub use agent::{Actor, AgentAction, AgentActionKind};
pub use artifact::{Artifact, ArtifactRelation, ArtifactRelationKind, ArtifactStatus, NewArtifact};
pub use comment::{Comment, CommentKind, CommentPatch, NewComment};
pub use complexity::{ComplexityHint, TaskBrief};
pub use device::Device;
pub use document::{Document, DocumentKind, NewDocument};
pub use evidence::{ActorRef, Evidence, EvidenceKind, NewEvidence};
pub use external_ref::ExternalRef;
pub use plan::{
    CanStart, CanStartBlocker, NewPlan, Plan, PlanFanoutWave, PlanGraph, PlanGraphEdge,
    PlanGraphNode, PlanPatch, PlanProgress, PlanProgressSummary, PlanStatus, PlanTask,
};
pub use project::{slugify_title, Project, DEFAULT_TENANT_ID};
pub use project_settings::{AutoAppendPatch, AutoAppendSettings};
pub use relation::{Relation, RelationKind, TaskRelations};
pub use rule::{
    Condition, NewRule, Requirement, Rule, RuleMode, RulePatch, RuleScope, RuleTrigger,
};
pub use run::{Run, RunNote, RunOutcome, RunStatus};
pub use session::{
    AgentSession, AgentSessionPlanStep, SessionArtifact, SessionArtifactKind, SessionStepStatus,
};
pub use signal::SignalKind;
pub use task::{CompletionNote, NewTask, Priority, Status, Task, TaskPatch, TriageState};
pub use work_lease::{canonical_target_uri, targets_overlap, LeaseMode, WorkLease};
pub use work_unit::{NewWorkUnit, WorkUnit, WorkUnitStatus};
