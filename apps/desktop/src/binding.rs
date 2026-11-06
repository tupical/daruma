//! Which server each local replica belongs to.
//!
//! A cursor only means something in one server's log, and a queued event was
//! written for one server. So a replica belongs to one server for good, named
//! by the id that server reports, and working with another server selects
//! another replica instead of repointing this one. Nothing moves between
//! replicas, so a change of server cannot lose or leak anything.
//!
//! `replicas.json` in the data directory says which replica is active, and is
//! replaced atomically. `paired.json` says which server the user chose last:
//! when the two disagree, as after a `pair` stopped halfway, the next open
//! follows `paired.json`, so a switch is finished by whichever process comes
//! next. A replica without a recorded server stays put until the user decides.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use daruma_shared::{CoreError, Result as CoreResult};
use serde::{Deserialize, Serialize};

use crate::outbox::{PENDING_COUNT_SQL, REPLICA_CURSOR_SQL};

/// The database of the first server, and of a replica from before bindings.
pub const DEFAULT_REPLICA: &str = "replica.sqlite";
const REGISTRY_FILE: &str = "replicas.json";
const SERVERS_DIR: &str = "servers";

/// Identity a server reports for its event log. It names a directory here, so
/// it never gets past this check with a path in it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ServerId(String);

impl ServerId {
    pub fn parse(raw: &str) -> CoreResult<Self> {
        let valid = (1..=80).contains(&raw.len())
            && raw
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
        if !valid {
            return Err(CoreError::validation(format!(
                "server reported an unusable identity: {raw:?}"
            )));
        }
        Ok(Self(raw.to_owned()))
    }
}

impl TryFrom<String> for ServerId {
    type Error = String;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::parse(&raw).map_err(|e| e.to_string())
    }
}

impl From<ServerId> for String {
    fn from(id: ServerId) -> Self {
        id.0
    }
}

impl std::fmt::Display for ServerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Contents of `replicas.json`. `active: None` means no server was ever
/// recorded here: the default file may be empty or a replica from before
/// bindings.
#[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Registry {
    active: Option<ServerId>,
    /// The server whose replica is the default file.
    #[serde(default)]
    default_owner: Option<ServerId>,
}

impl Registry {
    /// A missing file is a data directory from before bindings; an unreadable
    /// one stops here, since guessing could open another server's replica.
    pub fn load(dir: &Path) -> anyhow::Result<Self> {
        let path = dir.join(REGISTRY_FILE);
        match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("{} is unreadable", path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
        }
    }

    pub fn save(&self, dir: &Path) -> anyhow::Result<()> {
        write_private(&dir.join(REGISTRY_FILE), &serde_json::to_vec_pretty(self)?)
    }

    pub fn active(&self) -> Option<&ServerId> {
        self.active.as_ref()
    }

    /// Follow the server the user paired with; true if anything changed. Until
    /// a server is recorded this waits for [`Self::bind_default`], which needs
    /// to know what the default file holds.
    pub fn follow(&mut self, server: &ServerId) -> bool {
        if self.active.is_none() || self.active.as_ref() == Some(server) {
            return false;
        }
        self.active = Some(server.clone());
        true
    }

    /// Record that the default file is `server`'s replica.
    pub fn bind_default(&mut self, server: &ServerId) {
        self.active = Some(server.clone());
        self.default_owner = Some(server.clone());
    }

    /// The database of the active replica.
    pub fn active_path(&self, dir: &Path) -> PathBuf {
        match &self.active {
            Some(server) if self.default_owner.as_ref() != Some(server) => server_path(dir, server),
            _ => dir.join(DEFAULT_REPLICA),
        }
    }

    /// Every replica on disk, active first, the default file before the rest.
    pub fn replicas(&self, dir: &Path) -> anyhow::Result<Vec<ReplicaEntry>> {
        let mut entries = Vec::new();
        if dir.join(DEFAULT_REPLICA).exists() {
            entries.push(ReplicaEntry {
                server: self.default_owner.clone(),
                path: dir.join(DEFAULT_REPLICA),
            });
        }
        let servers = dir.join(SERVERS_DIR);
        let mut parked = Vec::new();
        if servers.exists() {
            for entry in std::fs::read_dir(&servers).context("read replicas")? {
                let name = entry.context("read replicas")?.file_name();
                let Some(server) = name.to_str().and_then(|n| ServerId::parse(n).ok()) else {
                    continue;
                };
                let path = server_path(dir, &server);
                if path.exists() {
                    parked.push(ReplicaEntry {
                        server: Some(server),
                        path,
                    });
                }
            }
        }
        parked.sort_by(|a, b| a.server.cmp(&b.server));
        entries.append(&mut parked);
        let active = self.active_path(dir);
        entries.sort_by_key(|entry| entry.path != active);
        Ok(entries)
    }
}

fn server_path(dir: &Path, server: &ServerId) -> PathBuf {
    dir.join(SERVERS_DIR).join(&server.0).join(DEFAULT_REPLICA)
}

#[derive(Debug, PartialEq, Eq)]
pub struct ReplicaEntry {
    /// `None`: a replica from before bindings, its server unknown.
    pub server: Option<ServerId>,
    pub path: PathBuf,
}

/// What a replica holds, read without touching it; `None` if it has no state
/// yet.
pub async fn replica_summary(path: &Path) -> Option<(i64, i64)> {
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(path)
        .read_only(true);
    let pool = sqlx::SqlitePool::connect_with(options).await.ok()?;
    let cursor = sqlx::query_as::<_, (i64,)>(REPLICA_CURSOR_SQL)
        .fetch_one(&pool)
        .await
        .ok()?;
    let pending = sqlx::query_as::<_, (i64,)>(PENDING_COUNT_SQL)
        .fetch_one(&pool)
        .await
        .ok()?;
    pool.close().await;
    Some((cursor.0, pending.0))
}

/// Replace `path` with `bytes` all at once and readable by the owner only, so
/// a crash leaves the old file or the new one, never a torn one.
pub fn write_private(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    use std::io::Write;

    let dir = path.parent().context("path has no parent")?;
    std::fs::create_dir_all(dir).context("create data dir")?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .context("path has no file name")?;
    let temporary = dir.join(format!(".{name}.{}", std::process::id()));

    let mut options = std::fs::OpenOptions::new();
    options.create(true).write(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temporary)
        .with_context(|| format!("create {}", temporary.display()))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .with_context(|| format!("write {}", temporary.display()))?;
    drop(file);
    std::fs::rename(&temporary, path).with_context(|| format!("replace {}", path.display()))?;
    #[cfg(unix)]
    std::fs::File::open(dir)
        .and_then(|d| d.sync_all())
        .with_context(|| format!("sync {}", dir.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(id: &str) -> ServerId {
        ServerId::parse(id).unwrap()
    }

    fn temp_dir() -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("daruma-binding-{}", daruma_shared::DeviceId::new()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn server_id_with_a_path_in_it_never_names_a_directory() {
        for raw in ["", "..", "../evil", "a/b", "a\\b", "a b", "a.b", "id\n"] {
            assert!(ServerId::parse(raw).is_err(), "{raw:?}");
        }
        assert!(ServerId::parse(&"a".repeat(81)).is_err());
        assert!(serde_json::from_str::<ServerId>("\"../evil\"").is_err());
        assert!(serde_json::from_str::<Registry>("{\"active\":\"../evil\"}").is_err());
        ServerId::parse("0123abcDEF_-").unwrap();
    }

    #[test]
    fn nothing_recorded_keeps_the_default_file_until_it_is_bound() {
        let dir = Path::new("/data");
        let mut registry = Registry::default();

        assert!(!registry.follow(&server("a")));
        assert_eq!(registry.active(), None);
        assert_eq!(registry.active_path(dir), dir.join("replica.sqlite"));

        registry.bind_default(&server("a"));
        assert_eq!(registry.active(), Some(&server("a")));
        assert_eq!(registry.active_path(dir), dir.join("replica.sqlite"));
    }

    #[test]
    fn another_server_gets_its_own_file_and_the_first_gets_its_own_back() {
        let dir = Path::new("/data");
        let mut registry = Registry::default();
        registry.bind_default(&server("a"));

        assert!(!registry.follow(&server("a")), "same server: no change");
        assert!(registry.follow(&server("b")));
        assert_eq!(
            registry.active_path(dir),
            dir.join("servers/b/replica.sqlite")
        );
        assert!(registry.follow(&server("a")));
        assert_eq!(registry.active_path(dir), dir.join("replica.sqlite"));
        assert!(registry.follow(&server("c")));
        assert_eq!(
            registry.active_path(dir),
            dir.join("servers/c/replica.sqlite")
        );
    }

    #[test]
    fn registry_survives_a_reload_and_a_broken_one_stops_the_open() {
        let dir = temp_dir();
        assert_eq!(Registry::load(&dir).unwrap(), Registry::default());

        let mut registry = Registry::default();
        registry.bind_default(&server("a"));
        registry.follow(&server("b"));
        registry.save(&dir).unwrap();
        assert_eq!(Registry::load(&dir).unwrap(), registry);

        std::fs::write(dir.join(REGISTRY_FILE), b"{\"active\":").unwrap();
        assert!(Registry::load(&dir).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn replicas_list_the_active_one_first_and_mark_the_unknown_one() {
        let dir = temp_dir();
        std::fs::write(dir.join(DEFAULT_REPLICA), b"").unwrap();
        for id in ["b", "c"] {
            let path = server_path(&dir, &server(id));
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"").unwrap();
        }
        // Not a server's directory: a stray name stays out of the list.
        std::fs::create_dir_all(dir.join("servers/not.a.server")).unwrap();

        let unknown = Registry::default().replicas(&dir).unwrap();
        assert_eq!(unknown[0].server, None);
        assert_eq!(unknown.len(), 3);

        let mut registry = Registry::default();
        registry.bind_default(&server("a"));
        registry.follow(&server("c"));
        let listed = registry.replicas(&dir).unwrap();
        let servers: Vec<_> = listed.iter().map(|e| e.server.clone().unwrap()).collect();
        assert_eq!(servers, [server("c"), server("a"), server("b")]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn write_private_replaces_the_file_for_the_owner_only() {
        let dir = temp_dir();
        let path = dir.join("nested").join("paired.json");
        write_private(&path, b"first").unwrap();
        write_private(&path, b"second").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"second");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(leftovers.len(), 1, "{leftovers:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
