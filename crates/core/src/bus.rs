//! `CommandBus` — thin async facade over [`CommandHandler`].
//!
//! Transports (HTTP, WS, desktop) call `dispatch`; the bus delegates to the
//! handler which owns all the heavy state (store, repos, event bus).

use std::sync::Arc;

use daruma_domain::Actor;
use daruma_events::EventEnvelope;
use daruma_shared::Result;

use crate::{lifecycle_gate::DispatchOutcome, Command, CommandHandler};

/// Entry point for every command in the system.
///
/// Clone freely — the inner [`CommandHandler`] is reference-counted.
#[derive(Clone)]
pub struct CommandBus {
    handler: Arc<CommandHandler>,
}

impl CommandBus {
    pub fn new(handler: Arc<CommandHandler>) -> Self {
        Self { handler }
    }

    /// Clone out the underlying handler — used by background watchdog
    /// tasks (liveness, due-date) and integration tests.
    pub fn handler(&self) -> Arc<CommandHandler> {
        self.handler.clone()
    }

    /// Dispatch a command and return the persisted event envelopes.
    pub async fn dispatch(&self, cmd: Command, actor: Actor) -> Result<Vec<EventEnvelope>> {
        self.handler.handle(cmd, actor).await
    }

    /// Dispatch a command and additionally return lifecycle-gate warnings so
    /// transports can surface them in `MutationResponse.warnings`
    /// (docs/LIFECYCLE_RULES_SPEC.md §1.5). [`Self::dispatch`] discards them.
    pub async fn dispatch_with_warnings(
        &self,
        cmd: Command,
        actor: Actor,
    ) -> Result<DispatchOutcome> {
        self.handler.handle_with_warnings(cmd, actor).await
    }
}
