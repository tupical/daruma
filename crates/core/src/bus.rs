//! `CommandBus` — thin async facade over [`CommandHandler`].
//!
//! Transports (HTTP, WS, desktop) call `dispatch`; the bus delegates to the
//! handler which owns all the heavy state (store, repos, event bus).

use std::sync::Arc;

use taskagent_domain::Actor;
use taskagent_events::EventEnvelope;
use taskagent_shared::Result;

use crate::{Command, CommandHandler};

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

    /// Dispatch a command and return the persisted event envelopes.
    pub async fn dispatch(&self, cmd: Command, actor: Actor) -> Result<Vec<EventEnvelope>> {
        self.handler.handle(cmd, actor).await
    }
}
