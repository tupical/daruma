//! Shared runtime context for the desktop CLI.
//!
//! Opens the local SQLite database and wires the same command / event /
//! AI stack the server uses. Local-first: the CLI does not talk to the
//! server; it embeds the engine directly.

use std::{path::PathBuf, sync::Arc};

use daruma_ai::{AiConfig, OpenAiClient};
// §3.4 W2.1: embed-mode reaches for the runtime through `core::embed`
// only — never via `daruma_storage::*` or `daruma_events::*`
// directly, so the W4.1 audit-grep step can keep enforcing
// "modules do not depend on core internals".
use daruma_core::embed::{
    ActivityRepo, CommandBus, CommandHandler, CommentRepo, Db, EventBus, EventStore, ProjectRepo,
    SqliteEventStore, TaskRepo,
};

use crate::{local_executor::LocalExecutor, outbox::Outbox, replica::Replica};

/// Fields beyond `tasks` / `commands` / `ai` are retained for the upcoming
/// GPUI client; allow `dead_code` until they are wired up.
#[allow(dead_code)]
pub struct Context {
    pub store: Arc<dyn EventStore>,
    pub tasks: Arc<TaskRepo>,
    pub projects: Arc<ProjectRepo>,
    pub comments: Arc<CommentRepo>,
    pub activity: Arc<ActivityRepo>,
    pub commands: CommandBus,
    pub local: LocalExecutor,
    pub replica: Replica,
    pub ai: Option<OpenAiClient>,
    pub db_path: PathBuf,
}

impl Context {
    pub async fn open() -> anyhow::Result<Self> {
        let db_path = data_path();
        if let Some(parent) = db_path.parent() {
            tokio::fs::create_dir_all(parent).await.ok();
        }
        let db_str = db_path
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("DB path contains non-UTF-8 characters"))?;

        let db = Db::open(db_str)
            .await
            .map_err(|e| anyhow::anyhow!("open db: {e}"))?;
        db.migrate()
            .await
            .map_err(|e| anyhow::anyhow!("migrate db: {e}"))?;

        let pool = db.pool().clone();
        let store: Arc<dyn EventStore> = Arc::new(SqliteEventStore::new(pool.clone()));
        let tasks = Arc::new(TaskRepo::new(pool.clone()));
        let projects = Arc::new(ProjectRepo::new(pool.clone()));
        let comments = Arc::new(CommentRepo::new(pool.clone()));
        let activity = Arc::new(ActivityRepo::new(pool.clone()));

        let bus = EventBus::new(256);
        let handler = Arc::new(CommandHandler::new(
            store.clone(),
            tasks.clone(),
            projects.clone(),
            comments.clone(),
            activity.clone(),
            bus,
        ));
        let commands = CommandBus::new(handler);
        let outbox = Outbox::new(db);
        outbox
            .ensure_schema()
            .await
            .map_err(|e| anyhow::anyhow!("outbox schema: {e}"))?;
        let replica = Replica::new(
            pool.clone(),
            store.clone(),
            tasks.clone(),
            projects.clone(),
            comments.clone(),
            activity.clone(),
        );
        replica
            .ensure_schema()
            .await
            .map_err(|e| anyhow::anyhow!("replica schema: {e}"))?;
        let local = LocalExecutor::new(commands.clone(), outbox)
            .await
            .map_err(|e| anyhow::anyhow!("local executor: {e}"))?;

        let ai = AiConfig::from_env().ok().map(OpenAiClient::new);

        Ok(Self {
            store,
            tasks,
            projects,
            comments,
            activity,
            commands,
            local,
            replica,
            ai,
            db_path,
        })
    }
}

/// Resolve the local DB path.
///
/// Precedence:
///   1. `DARUMA_DATA_DIR` env var → `<dir>/replica.sqlite`
///   2. current working directory → `./replica.sqlite`
pub fn data_path() -> PathBuf {
    let dir = std::env::var("DARUMA_DATA_DIR").unwrap_or_else(|_| ".".into());
    PathBuf::from(dir).join("replica.sqlite")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    #[test]
    fn data_path_uses_replica_sqlite_name() {
        let _guard = env_lock().lock().unwrap();
        std::env::set_var("DARUMA_DATA_DIR", "/tmp/daruma-desktop-test");
        assert_eq!(
            data_path(),
            PathBuf::from("/tmp/daruma-desktop-test/replica.sqlite")
        );
        std::env::remove_var("DARUMA_DATA_DIR");
    }
}
