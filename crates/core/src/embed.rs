//! Public surface for **embed-mode** consumers (§3.4 W2.1).
//!
//! Embed clients (today: `apps/desktop`) run the same `taskagent-core`
//! runtime in their own process, with no network in the data path.
//! They must reach for runtime types through this module — never via
//! the internal `taskagent_storage` / `taskagent_events` paths — so the
//! "modules don't depend on internals" rule from
//! [docs/MODULE_CONTRACT.md](../../docs/MODULE_CONTRACT.md) stays
//! verifiable by `grep` (W4.1 audit step).
//!
//! Semantics are identical to the network path: commands go through
//! [`CommandBus::dispatch`], events come back through [`EventBus`], and
//! repos are projections over the same [`EventStore`].

pub use crate::{Command, CommandBus, CommandEnvelope, CommandHandler};
pub use taskagent_events::{Event, EventBus, EventEnvelope, EventStore};
pub use taskagent_storage::{
    ActivityRepo, CommentRepo, Db, ProjectRepo, SqliteEventStore, TaskRepo,
};
