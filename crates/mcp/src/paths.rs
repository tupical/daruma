//! On-disk layout for the MCP client (`daruma-mcp`).
//!
//! All local state lives under [`agent_dir`] (default `~/.agents/daruma/`):
//!
//! ```text
//! ~/.agents/daruma/
//!   workspaces.json   # workspace key → default project id
//!   credentials.json  # remote/self-host profiles (CLI install)
//!   data/             # server SQLite (DARUMA_DATA_DIR default)
//!     daruma.sqlite
//!     workspacegraph.sqlite
//!     bootstrap.token
//! ```
//!
//! Override the directory with `DARUMA_AGENT_DIR`, or the file alone with
//! `DARUMA_WORKSPACES_FILE`.

use std::path::{Path, PathBuf};

/// Root directory for MCP client state (not server SQLite — see `DARUMA_DATA_DIR`).
pub const ENV_AGENT_DIR: &str = "DARUMA_AGENT_DIR";

/// Full path to `workspaces.json` when set; overrides [`workspaces_file`].
pub const ENV_WORKSPACES_FILE: &str = "DARUMA_WORKSPACES_FILE";

/// Override directory for server SQLite (`daruma.sqlite`, `bootstrap.token`, …).
pub const ENV_DATA_DIR: &str = "DARUMA_DATA_DIR";

/// Canonical server data directory: [`agent_dir`] + `data/`, or `DARUMA_DATA_DIR` when set.
pub fn data_dir() -> PathBuf {
    if let Ok(dir) = std::env::var(ENV_DATA_DIR) {
        return PathBuf::from(dir);
    }
    agent_dir().join("data")
}

/// Default agent data root: `$HOME/.agents/daruma`, or `./.agents/daruma` if `HOME` is unset.
pub fn agent_dir() -> PathBuf {
    if let Ok(dir) = std::env::var(ENV_AGENT_DIR) {
        return PathBuf::from(dir);
    }
    std::env::var("HOME")
        .map(|h| PathBuf::from(h).join(".agents").join("daruma"))
        .unwrap_or_else(|_| PathBuf::from(".agents").join("daruma"))
}

/// Canonical `workspaces.json` path.
pub fn workspaces_file() -> PathBuf {
    if let Ok(path) = std::env::var(ENV_WORKSPACES_FILE) {
        return PathBuf::from(path);
    }
    agent_dir().join("workspaces.json")
}

/// Previous default locations (migrated once when the canonical file is absent).
pub fn legacy_workspaces_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Ok(home) = std::env::var("HOME") {
        let home = PathBuf::from(&home);
        paths.push(home.join(".config").join("daruma").join("workspaces.json"));
    }
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        paths.push(PathBuf::from(xdg).join("daruma").join("workspaces.json"));
    }
    if let Ok(cwd) = std::env::current_dir() {
        paths.push(cwd.join("daruma").join("workspaces.json"));
    }
    paths
}

/// Copy the first existing legacy `workspaces.json` into `target` if `target` is missing.
pub fn migrate_legacy_workspaces(target: &Path) -> std::io::Result<bool> {
    if target.exists() {
        return Ok(false);
    }
    for legacy in legacy_workspaces_paths() {
        if legacy.is_file() {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(&legacy, target)?;
            tracing::info!(
                from = %legacy.display(),
                to = %target.display(),
                "migrated workspaces.json into agent data dir"
            );
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::env_lock;

    #[test]
    fn agent_dir_defaults_under_home_agents_daruma() {
        let _guard = env_lock();
        std::env::remove_var(ENV_AGENT_DIR);
        std::env::set_var("HOME", "/tmp/daruma-test-home");
        assert_eq!(
            agent_dir(),
            PathBuf::from("/tmp/daruma-test-home/.agents/daruma")
        );
        std::env::remove_var("HOME");
    }

    #[test]
    fn workspaces_file_honours_agent_dir() {
        let _guard = env_lock();
        std::env::set_var(ENV_AGENT_DIR, "/var/daruma-agent");
        std::env::remove_var(ENV_WORKSPACES_FILE);
        assert_eq!(
            workspaces_file(),
            PathBuf::from("/var/daruma-agent/workspaces.json")
        );
        std::env::remove_var(ENV_AGENT_DIR);
    }

    #[test]
    fn workspaces_file_env_overrides_dir() {
        let _guard = env_lock();
        std::env::set_var(ENV_WORKSPACES_FILE, "/custom/ws.json");
        assert_eq!(workspaces_file(), PathBuf::from("/custom/ws.json"));
        std::env::remove_var(ENV_WORKSPACES_FILE);
    }

    #[test]
    fn data_dir_defaults_under_agent_data() {
        let _guard = env_lock();
        std::env::remove_var(ENV_DATA_DIR);
        std::env::remove_var(ENV_AGENT_DIR);
        std::env::set_var("HOME", "/tmp/daruma-test-home");
        assert_eq!(
            data_dir(),
            PathBuf::from("/tmp/daruma-test-home/.agents/daruma/data")
        );
        std::env::remove_var("HOME");
    }

    #[test]
    fn data_dir_env_overrides_default() {
        let _guard = env_lock();
        std::env::set_var(ENV_DATA_DIR, "/var/daruma-data");
        assert_eq!(data_dir(), PathBuf::from("/var/daruma-data"));
        std::env::remove_var(ENV_DATA_DIR);
    }
}
