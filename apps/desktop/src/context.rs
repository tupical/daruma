//! Shared runtime context for the desktop CLI.
//!
//! Opens the local SQLite database and wires the same command / event /
//! AI stack the server uses. Local-first: the CLI does not talk to the
//! server; it embeds the engine directly.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use daruma_ai_infra::{AiConfig, OpenAiClient};
// §3.4 W2.1: embed-mode reaches for the runtime through `core::embed`
// only — never via `daruma_storage::*` or `daruma_events::*`
// directly, so the W4.1 audit-grep step can keep enforcing
// "modules do not depend on core internals".
use daruma_core::embed::{
    ActivityRepo, CommandBus, CommandHandler, CommentRepo, Db, EventBus, EventStore, ProjectRepo,
    SqliteEventStore, TaskRepo,
};

use crate::{
    binding::{Registry, ServerId, DEFAULT_REPLICA},
    local_executor::LocalExecutor,
    outbox::Outbox,
    remote,
    replica::Replica,
};

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
    /// The server this open replica belongs to; `None` if none is recorded.
    server: Option<ServerId>,
    dir: PathBuf,
}

impl Context {
    pub async fn open() -> anyhow::Result<Self> {
        Self::open_in(data_dir(), remote::paired_server_id()?).await
    }

    /// Open the replica of `paired`, the server the user last paired with.
    pub(crate) async fn open_in(dir: PathBuf, paired: Option<ServerId>) -> anyhow::Result<Self> {
        let (mut registry, followed) = selected_registry(&dir, paired.as_ref())?;
        if followed {
            registry.save(&dir)?;
        }
        let db_path = registry.active_path(&dir);
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

        let outbox = Outbox::new(db);
        outbox
            .ensure_schema()
            .await
            .map_err(|e| anyhow::anyhow!("outbox schema: {e}"))?;
        let outbound = outbox
            .outbound_store()
            .await
            .map_err(|e| anyhow::anyhow!("outbox store: {e}"))?;

        let bus = EventBus::new(256);
        let handler = Arc::new(CommandHandler::new(
            Arc::new(outbound),
            tasks.clone(),
            projects.clone(),
            comments.clone(),
            activity.clone(),
            bus,
        ));
        let commands = CommandBus::new(handler);
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
        let local = LocalExecutor::new(commands.clone(), outbox);

        let ai = AiConfig::from_env().ok().map(OpenAiClient::new);

        // A first server takes over the default file unless the file already
        // has a history with some server: then `sync` asks the user.
        if let (None, Some(server)) = (registry.active(), paired.as_ref()) {
            let synced = local
                .has_synced()
                .await
                .map_err(|e| anyhow::anyhow!(e.to_string()))?;
            if !synced {
                registry.bind_default(server);
                registry.save(&dir)?;
            }
        }
        let server = registry.active().cloned();

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
            server,
            dir,
        })
    }

    /// Refuse a server this replica does not belong to, before anything is
    /// sent or fetched. `url` only makes the message concrete: the check is on
    /// `server`, which is what the server itself reported.
    pub async fn ensure_bound(&self, server: &ServerId, url: &str) -> anyhow::Result<()> {
        match &self.server {
            Some(bound) if bound == server => Ok(()),
            Some(bound) => anyhow::bail!(
                "this replica belongs to server {bound}, but {url} is server {server}. \
                 Nothing was sent or fetched. To work with {server}, run `daruma pair`: it \
                 gets its own replica and this one stays as it is. Or set DARUMA_DATA_DIR to \
                 another directory for it."
            ),
            None if self.has_synced().await? => anyhow::bail!(
                "this replica has synced before, but it does not record with which server, \
                 and {url} is server {server}. Nothing was sent or fetched. If it has only \
                 ever synced with this server, run `daruma replica adopt`. If not, set \
                 DARUMA_DATA_DIR to another directory for this server and keep this one."
            ),
            None => self.record_default_owner(server),
        }
    }

    /// `daruma replica adopt`: the user's word that the unrecorded replica
    /// is `server`'s. It has to be: its cursor means nothing in any other log.
    pub async fn adopt(&self, server: &ServerId) -> anyhow::Result<()> {
        if let Some(bound) = &self.server {
            anyhow::bail!("this replica already belongs to server {bound}");
        }
        self.record_default_owner(server)
    }

    /// `self.server` is `None`, so this process opened the default file; it
    /// only stays the right one while nothing else recorded a server.
    fn record_default_owner(&self, server: &ServerId) -> anyhow::Result<()> {
        let mut registry = Registry::load(&self.dir)?;
        if let Some(active) = registry.active() {
            anyhow::bail!("another process just selected server {active}; try again");
        }
        registry.bind_default(server);
        registry.save(&self.dir)
    }

    async fn has_synced(&self) -> anyhow::Result<bool> {
        self.local
            .has_synced()
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))
    }
}

/// The registry with the pairing applied, and whether that changed it.
fn selected_registry(dir: &Path, paired: Option<&ServerId>) -> anyhow::Result<(Registry, bool)> {
    let mut registry = Registry::load(dir)?;
    let changed = paired.is_some_and(|server| registry.follow(server));
    Ok((registry, changed))
}

/// The directory of everything local.
///
/// Precedence:
///   1. `DARUMA_DATA_DIR` env var
///   2. current working directory
pub fn data_dir() -> PathBuf {
    PathBuf::from(std::env::var("DARUMA_DATA_DIR").unwrap_or_else(|_| ".".into()))
}

/// The database a command would open now.
pub fn active_replica_path() -> anyhow::Result<PathBuf> {
    let dir = data_dir();
    let (registry, _) = selected_registry(&dir, remote::paired_server_id()?.as_ref())?;
    Ok(registry.active_path(&dir))
}

/// The default file of the data directory.
pub fn data_path() -> PathBuf {
    data_dir().join(DEFAULT_REPLICA)
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

    fn server(id: &str) -> ServerId {
        ServerId::parse(id).unwrap()
    }

    fn temp_dir() -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("daruma-context-{}", daruma_shared::DeviceId::new()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    async fn add(ctx: &Context, title: &str) {
        ctx.local
            .dispatch(
                daruma_core::embed::Command::CreateTask {
                    task: daruma_domain::NewTask::new(title),
                },
                daruma_domain::Actor::user(),
            )
            .await
            .unwrap();
    }

    async fn titles(ctx: &Context) -> Vec<String> {
        let mut titles: Vec<_> = ctx
            .tasks
            .list_all()
            .await
            .unwrap()
            .into_iter()
            .map(|task| task.title)
            .collect();
        titles.sort();
        titles
    }

    /// What a replica looks like after it read a server's log.
    async fn set_cursor(ctx: &Context, seq: i64) {
        sqlx::query("UPDATE desktop_replica_state SET server_seq = ? WHERE id = 1")
            .bind(seq)
            .execute(ctx.projects.pool())
            .await
            .unwrap();
    }

    fn refused(result: anyhow::Result<()>, why: &str) {
        let message = result.unwrap_err().to_string();
        assert!(message.contains(why), "{message}");
        assert!(message.contains("Nothing was sent or fetched"), "{message}");
    }

    #[tokio::test]
    async fn each_server_keeps_its_own_replica_across_a_change_and_back() {
        let dir = temp_dir();
        let a = Context::open_in(dir.clone(), Some(server("a")))
            .await
            .unwrap();
        a.ensure_bound(&server("a"), "https://a").await.unwrap();
        add(&a, "for a").await;
        set_cursor(&a, 7).await;
        let a_path = a.db_path.clone();
        drop(a);

        let b = Context::open_in(dir.clone(), Some(server("b")))
            .await
            .unwrap();
        assert_ne!(b.db_path, a_path);
        b.ensure_bound(&server("b"), "https://b").await.unwrap();
        assert!(titles(&b).await.is_empty(), "nothing of a's reached b");
        assert_eq!(
            b.replica.server_seq().await.unwrap(),
            0,
            "b starts from its own log"
        );
        add(&b, "for b").await;
        drop(b);

        let back = Context::open_in(dir.clone(), Some(server("a")))
            .await
            .unwrap();
        assert_eq!(back.db_path, a_path);
        assert_eq!(titles(&back).await, ["for a"]);
        assert_eq!(back.replica.server_seq().await.unwrap(), 7);
        drop(back);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn another_token_for_the_same_server_changes_nothing() {
        let dir = temp_dir();
        let first = Context::open_in(dir.clone(), Some(server("a")))
            .await
            .unwrap();
        add(&first, "kept").await;
        drop(first);
        let again = Context::open_in(dir.clone(), Some(server("a")))
            .await
            .unwrap();
        assert_eq!(titles(&again).await, ["kept"]);
        assert_eq!(again.db_path, dir.join("replica.sqlite"));
        drop(again);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn a_server_that_is_not_this_replicas_is_refused() {
        let dir = temp_dir();
        let ctx = Context::open_in(dir.clone(), Some(server("a")))
            .await
            .unwrap();
        refused(
            ctx.ensure_bound(&server("b"), "https://override").await,
            "belongs to server a",
        );
        drop(ctx);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn an_open_replica_stays_refused_after_a_pair_elsewhere_moved_on() {
        let dir = temp_dir();
        let a = Context::open_in(dir.clone(), Some(server("a")))
            .await
            .unwrap();
        // A `pair` in another process, while this one still holds a's replica.
        let b = Context::open_in(dir.clone(), Some(server("b")))
            .await
            .unwrap();
        drop(b);

        refused(
            a.ensure_bound(&server("b"), "https://b").await,
            "belongs to server a",
        );
        drop(a);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn a_replica_nothing_has_seen_goes_with_the_first_server() {
        let dir = temp_dir();
        let ctx = Context::open_in(dir.clone(), None).await.unwrap();
        add(&ctx, "written offline").await;
        ctx.ensure_bound(&server("a"), "https://a").await.unwrap();
        assert_eq!(Registry::load(&dir).unwrap().active(), Some(&server("a")));
        drop(ctx);

        let reopened = Context::open_in(dir.clone(), None).await.unwrap();
        refused(
            reopened.ensure_bound(&server("b"), "https://b").await,
            "belongs to server a",
        );
        drop(reopened);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn a_replica_with_a_past_and_no_record_waits_for_the_user() {
        let dir = temp_dir();
        let legacy = Context::open_in(dir.clone(), None).await.unwrap();
        add(&legacy, "written for some server").await;
        set_cursor(&legacy, 3).await;
        drop(legacy);

        // Pairing does not decide for the user, whichever server it was.
        let paired = Context::open_in(dir.clone(), Some(server("b")))
            .await
            .unwrap();
        assert_eq!(paired.db_path, dir.join("replica.sqlite"));
        refused(
            paired.ensure_bound(&server("b"), "https://b").await,
            "does not record with which server",
        );
        assert_eq!(Registry::load(&dir).unwrap().active(), None);

        paired.adopt(&server("b")).await.unwrap();
        drop(paired);
        let adopted = Context::open_in(dir.clone(), Some(server("b")))
            .await
            .unwrap();
        adopted
            .ensure_bound(&server("b"), "https://b")
            .await
            .unwrap();
        assert_eq!(titles(&adopted).await, ["written for some server"]);
        assert!(
            adopted.adopt(&server("c")).await.is_err(),
            "no second owner"
        );
        drop(adopted);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn a_pair_stopped_before_the_registry_is_finished_by_the_next_open() {
        let dir = temp_dir();
        let a = Context::open_in(dir.clone(), Some(server("a")))
            .await
            .unwrap();
        add(&a, "for a").await;
        let a_path = a.db_path.clone();
        drop(a);

        // `paired.json` already names b, `replicas.json` still says a.
        let b = Context::open_in(dir.clone(), Some(server("b")))
            .await
            .unwrap();
        assert_ne!(b.db_path, a_path);
        assert!(titles(&b).await.is_empty());
        assert_eq!(Registry::load(&dir).unwrap().active(), Some(&server("b")));
        drop(b);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn an_unreadable_registry_opens_nothing() {
        let dir = temp_dir();
        std::fs::write(dir.join("replicas.json"), b"{").unwrap();
        assert!(Context::open_in(dir.clone(), Some(server("a")))
            .await
            .is_err());
        assert!(!dir.join("replica.sqlite").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
