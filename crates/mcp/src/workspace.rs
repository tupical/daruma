//! Per-workspace defaults persisted to [`crate::paths::workspaces_file`]
//! (default `~/.agents/daruma/workspaces.json`).
//!
//! Lets the MCP client say "for this workspace, my default project is X"
//! once, so subsequent `daruma_list`/`daruma_create` calls don't
//! need to repeat the `project_id`. The map is keyed by an opaque
//! workspace identifier — by default the CWD at MCP startup, but the
//! caller may override via `DARUMA_WORKSPACE`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

use crate::paths;

/// In-memory + on-disk store of `workspace → default project`.
pub struct Workspace {
    key: String,
    path: PathBuf,
    inner: Mutex<Inner>,
}

#[derive(Default, Serialize, Deserialize)]
struct Inner {
    /// `workspace_key → project_id`. `BTreeMap` for stable on-disk diffs.
    workspaces: BTreeMap<String, String>,
}

impl Workspace {
    /// Initialize the global workspace state. Reads the workspace key from
    /// `DARUMA_WORKSPACE` env, falling back to the current working
    /// directory. Loads any existing state from disk.
    pub fn init() -> Self {
        let key = std::env::var("DARUMA_WORKSPACE")
            .ok()
            .or_else(|| {
                std::env::current_dir()
                    .ok()
                    .map(|p| p.to_string_lossy().into_owned())
            })
            .unwrap_or_else(|| "default".to_string());

        let path = paths::workspaces_file();
        let _ = paths::migrate_legacy_workspaces(&path);
        let inner = load(&path).unwrap_or_default();
        Self {
            key,
            path,
            inner: Mutex::new(inner),
        }
    }

    /// Return the workspace key the MCP binary was launched in.
    pub fn key(&self) -> &str {
        &self.key
    }

    /// Session-wide override from `DARUMA_PROJECT_ID`.
    pub fn env_project(&self) -> Option<String> {
        if let Ok(p) = std::env::var("DARUMA_PROJECT_ID") {
            if !p.is_empty() {
                return Some(p);
            }
        }
        None
    }

    /// Return every configured workspace/scope mapping.
    pub fn scopes(&self) -> Vec<(String, String)> {
        self.inner
            .lock()
            .map(|g| {
                g.workspaces
                    .iter()
                    .filter(|(_, project_id)| !project_id.is_empty())
                    .map(|(scope, project_id)| (scope.clone(), project_id.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Resolve a named scope. Names may be the exact configured key or its
    /// final path component (e.g. `daruma-secondary`).
    pub fn project_for_scope(&self, scope: &str) -> anyhow::Result<Option<String>> {
        let scope = scope.trim();
        if scope.is_empty() {
            return Ok(None);
        }
        let scopes = self.scopes();
        if let Some((_, project_id)) = scopes.iter().find(|(key, _)| key == scope) {
            return Ok(Some(project_id.clone()));
        }
        let matches = scopes
            .iter()
            .filter(|(key, _)| scope_name(key).is_some_and(|name| name == scope))
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [] => Ok(None),
            [(_, project_id)] => Ok(Some((*project_id).clone())),
            _ => anyhow::bail!(
                "ambiguous daruma scope `{scope}`; use `scope_path` or `project_id`"
            ),
        }
    }

    /// Resolve by filesystem path using the longest configured path prefix.
    pub fn project_for_path(&self, path: &str) -> Option<String> {
        let path = self.resolve_path(path);
        self.scopes()
            .into_iter()
            .filter_map(|(key, project_id)| {
                let scope_path = normalize_path(&key);
                path_is_inside(&path, &scope_path).then_some((scope_path.len(), project_id))
            })
            .max_by_key(|(len, _)| *len)
            .map(|(_, project_id)| project_id)
    }

    /// Resolve the current workspace key. If the key is a parent directory of
    /// multiple configured scopes, return a friendly ambiguity error instead
    /// of silently falling back to unscoped project-less behavior.
    pub fn inferred_project(&self) -> anyhow::Result<Option<String>> {
        if let Some(project_id) = self.env_project() {
            return Ok(Some(project_id));
        }

        let key = normalize_path(&self.key);
        let nested = self
            .scopes()
            .into_iter()
            .filter(|(scope, _)| path_is_inside(&normalize_path(scope), &key))
            .collect::<Vec<_>>();
        if nested.len() > 1 {
            anyhow::bail!("{}", ambiguous_scope_message(&self.key, &nested));
        }
        if let Some(project_id) = self.project_for_path(&self.key) {
            return Ok(Some(project_id));
        }
        Ok(nested.into_iter().next().map(|(_, project_id)| project_id))
    }

    /// Best-effort current project for diagnostics.
    pub fn default_project(&self) -> Option<String> {
        self.inferred_project().ok().flatten()
    }

    /// Persist a new default project for this workspace.
    pub fn set_default_project(
        &self,
        project_id: &str,
        scope_path: Option<&str>,
    ) -> anyhow::Result<String> {
        let scope = match scope_path {
            Some(path) => self.resolve_path(path),
            None => self.scope_for_current_workspace()?,
        };
        let mut g = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("workspace state poisoned"))?;
        if project_id.is_empty() {
            g.workspaces.remove(&scope);
        } else {
            g.workspaces.insert(scope.clone(), project_id.to_string());
        }
        save(&self.path, &g)?;
        Ok(scope)
    }

    fn resolve_path(&self, path: &str) -> String {
        let path = path.trim();
        if Path::new(path).is_absolute() {
            return normalize_path(path);
        }
        normalize_path(&Path::new(&self.key).join(path).to_string_lossy())
    }

    fn scope_for_current_workspace(&self) -> anyhow::Result<String> {
        let key = normalize_path(&self.key);
        let scopes = self.scopes();
        let child_scopes = scopes
            .iter()
            .filter(|(scope, _)| path_is_inside(&normalize_path(scope), &key))
            .collect::<Vec<_>>();
        if child_scopes.len() > 1 {
            anyhow::bail!(
                "current workspace `{}` contains multiple daruma scopes; pass `scope_path`",
                self.key
            );
        }
        let containing_scope = scopes
            .iter()
            .filter(|(scope, _)| path_is_inside(&key, &normalize_path(scope)))
            .max_by_key(|(scope, _)| normalize_path(scope).len());
        if let Some((scope, _)) = containing_scope {
            return Ok(normalize_path(scope));
        }
        Ok(key)
    }
}

fn scope_name(scope: &str) -> Option<&str> {
    Path::new(scope).file_name()?.to_str()
}

fn normalize_path(path: &str) -> String {
    let mut out = path.trim_end_matches('/').to_string();
    if out.is_empty() {
        out.push('/');
    }
    out
}

fn path_is_inside(path: &str, root: &str) -> bool {
    path == root
        || path
            .strip_prefix(root)
            .is_some_and(|rest| rest.starts_with('/'))
}

fn ambiguous_scope_message(workspace: &str, scopes: &[(String, String)]) -> String {
    let mut lines = vec![format!(
        "ambiguous daruma scope for `{workspace}`; pass `project_id`, `scope`, or `scope_path`"
    )];
    lines.push("known scopes:".to_string());
    for (scope, project_id) in scopes {
        lines.push(format!("- {scope} -> {project_id}"));
    }
    lines.join("\n")
}

/// Process-wide handle. Set once from `main.rs`.
static GLOBAL: OnceLock<Workspace> = OnceLock::new();

pub fn install(ws: Workspace) {
    let _ = GLOBAL.set(ws);
}

pub fn global() -> Option<&'static Workspace> {
    GLOBAL.get()
}

fn load(path: &PathBuf) -> Option<Inner> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

fn save(path: &PathBuf, inner: &Inner) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(inner)?;
    std::fs::write(path, text)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::env_lock;

    fn workspace(key: &str, scopes: &[(&str, &str)]) -> Workspace {
        let workspaces = scopes
            .iter()
            .map(|(scope, project_id)| (scope.to_string(), project_id.to_string()))
            .collect();
        Workspace {
            key: key.to_string(),
            path: std::env::temp_dir().join(format!(
                "daruma-workspaces-test-{}.json",
                uuid::Uuid::new_v4()
            )),
            inner: Mutex::new(Inner { workspaces }),
        }
    }

    #[test]
    fn project_for_path_uses_longest_matching_scope() {
        let _guard = env_lock();
        std::env::remove_var("DARUMA_PROJECT_ID");
        let ws = workspace(
            "/tmp/daruma-public-test/projects",
            &[
                ("/tmp/daruma-public-test/projects", "root"),
                (
                    "/tmp/daruma-public-test/projects/daruma-secondary",
                    "secondary",
                ),
            ],
        );

        assert_eq!(
            ws.project_for_path(
                "/tmp/daruma-public-test/projects/daruma-secondary/crates/api"
            ),
            Some("secondary".to_string())
        );
    }

    #[test]
    fn inferred_project_selects_single_repo_scope() {
        let _guard = env_lock();
        std::env::remove_var("DARUMA_PROJECT_ID");
        let ws = workspace(
            "/tmp/daruma-public-test/projects/daruma/crates/mcp",
            &[("/tmp/daruma-public-test/projects/daruma", "oss")],
        );

        assert_eq!(ws.inferred_project().unwrap(), Some("oss".to_string()));
    }

    #[test]
    fn inferred_project_errors_for_ambiguous_parent_scope_even_with_parent_mapping() {
        let _guard = env_lock();
        std::env::remove_var("DARUMA_PROJECT_ID");
        let ws = workspace(
            "/tmp/daruma-public-test/projects",
            &[
                ("/tmp/daruma-public-test/projects", "legacy-parent"),
                ("/tmp/daruma-public-test/projects/daruma", "oss"),
                (
                    "/tmp/daruma-public-test/projects/daruma-secondary",
                    "secondary",
                ),
            ],
        );

        let err = ws.inferred_project().unwrap_err().to_string();
        assert!(err.contains("ambiguous daruma scope"));
        assert!(err.contains("daruma-secondary"));
    }

    #[test]
    fn project_for_path_resolves_relative_path_from_workspace_key() {
        let _guard = env_lock();
        std::env::remove_var("DARUMA_PROJECT_ID");
        let ws = workspace(
            "/tmp/daruma-public-test/projects",
            &[(
                "/tmp/daruma-public-test/projects/daruma-secondary",
                "secondary",
            )],
        );

        assert_eq!(
            ws.project_for_path("daruma-secondary/crates/api"),
            Some("secondary".to_string())
        );
    }

    #[test]
    fn set_default_project_requires_scope_path_for_multi_repo_parent() {
        let _guard = env_lock();
        std::env::remove_var("DARUMA_PROJECT_ID");
        let ws = workspace(
            "/tmp/daruma-public-test/projects",
            &[
                ("/tmp/daruma-public-test/projects/daruma", "oss"),
                (
                    "/tmp/daruma-public-test/projects/daruma-secondary",
                    "secondary",
                ),
            ],
        );

        let err = ws.set_default_project("new", None).unwrap_err().to_string();
        assert!(err.contains("contains multiple daruma scopes"));

        let scope = ws
            .set_default_project("secondary-new", Some("daruma-secondary"))
            .unwrap();
        assert_eq!(
            scope,
            "/tmp/daruma-public-test/projects/daruma-secondary"
        );
        assert_eq!(
            ws.project_for_scope("daruma-secondary").unwrap(),
            Some("secondary-new".to_string())
        );
    }

    #[test]
    fn set_default_project_updates_existing_scope_from_subdir() {
        let _guard = env_lock();
        std::env::remove_var("DARUMA_PROJECT_ID");
        let ws = workspace(
            "/tmp/daruma-public-test/projects/daruma/crates/mcp",
            &[("/tmp/daruma-public-test/projects/daruma", "old")],
        );

        let scope = ws.set_default_project("new", None).unwrap();
        assert_eq!(scope, "/tmp/daruma-public-test/projects/daruma");
        assert_eq!(
            ws.project_for_scope("daruma").unwrap(),
            Some("new".to_string())
        );
        assert_eq!(ws.project_for_scope("mcp").unwrap(), None);
    }
}
