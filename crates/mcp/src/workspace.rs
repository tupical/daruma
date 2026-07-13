//! Repo scope resolution: `scope_path → default project`.
//!
//! Bindings live server-side in the `repo_scopes` table (`GET/PUT
//! /v1/repo-scopes`, migration 0046) so the same mapping works for local
//! stdio sessions and hosted (per-tenant) MCP sessions alike. This module
//! keeps only the local process context — the workspace key (CWD at MCP
//! startup, overridable via `DARUMA_WORKSPACE`) — plus pure resolution
//! logic over a fetched snapshot ([`ScopeView`]).
//!
//! The legacy file store (`~/.agents/daruma/workspaces.json`) is migrated
//! to the server once on stdio startup via [`migrate_workspaces_file`].

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::OnceLock;

use serde::Deserialize;
use serde_json::{json, Value};

use crate::client::ApiClient;
use crate::paths;

/// Local process context: just the workspace key. Installed once from the
/// stdio entry-point; absent in server mode (hosted MCP has no meaningful CWD).
pub struct Workspace {
    key: String,
}

impl Workspace {
    /// Read the workspace key from `DARUMA_WORKSPACE`, falling back to the
    /// current working directory.
    pub fn init() -> Self {
        let key = std::env::var("DARUMA_WORKSPACE")
            .ok()
            .or_else(|| {
                std::env::current_dir()
                    .ok()
                    .map(|p| p.to_string_lossy().into_owned())
            })
            .unwrap_or_else(|| "default".to_string());
        Self { key }
    }

    /// Return the workspace key the MCP binary was launched in.
    pub fn key(&self) -> &str {
        &self.key
    }
}

/// Session-wide override from `DARUMA_PROJECT_ID`.
pub fn env_project() -> Option<String> {
    std::env::var("DARUMA_PROJECT_ID")
        .ok()
        .filter(|p| !p.is_empty())
}

/// Process-wide handle. Set once from the stdio entry-point.
static GLOBAL: OnceLock<Workspace> = OnceLock::new();

pub fn install(ws: Workspace) {
    let _ = GLOBAL.set(ws);
}

pub fn global() -> Option<&'static Workspace> {
    GLOBAL.get()
}

/// Snapshot of the server-side scope bindings plus the local workspace key.
/// All resolution logic is pure over this snapshot.
pub struct ScopeView {
    key: Option<String>,
    scopes: Vec<(String, String)>,
}

impl ScopeView {
    pub fn new(key: Option<String>, scopes: Vec<(String, String)>) -> Self {
        Self { key, scopes }
    }

    /// Fetch the bindings from `GET /v1/repo-scopes`.
    pub async fn fetch(client: &ApiClient) -> anyhow::Result<Self> {
        let resp = client.get_json("/v1/repo-scopes").await?;
        let scopes = resp
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| {
                        let scope = item.get("scope_path")?.as_str()?;
                        let project_id = item.get("project_id")?.as_str()?;
                        (!project_id.is_empty())
                            .then(|| (scope.to_string(), project_id.to_string()))
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(Self::new(global().map(|w| w.key().to_string()), scopes))
    }

    /// Like [`ScopeView::fetch`], but degrades to an empty view when the
    /// server predates `/v1/repo-scopes` (or the fetch fails) — explicit
    /// `project_id` args keep working against older servers.
    pub async fn fetch_or_empty(client: &ApiClient) -> Self {
        Self::fetch(client)
            .await
            .unwrap_or_else(|_| Self::new(global().map(|w| w.key().to_string()), Vec::new()))
    }

    pub fn key(&self) -> Option<&str> {
        self.key.as_deref()
    }

    /// Every configured scope mapping as `(scope_path, project_id)`.
    pub fn scopes(&self) -> &[(String, String)] {
        &self.scopes
    }

    /// Resolve a named scope. Names may be the exact configured key or its
    /// final path component (e.g. `daruma-secondary`).
    pub fn project_for_scope(&self, scope: &str) -> anyhow::Result<Option<String>> {
        let scope = scope.trim();
        if scope.is_empty() {
            return Ok(None);
        }
        if let Some((_, project_id)) = self.scopes.iter().find(|(key, _)| key == scope) {
            return Ok(Some(project_id.clone()));
        }
        let matches = self
            .scopes
            .iter()
            .filter(|(key, _)| scope_name(key).is_some_and(|name| name == scope))
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [] => Ok(None),
            [(_, project_id)] => Ok(Some(project_id.clone())),
            _ => {
                anyhow::bail!("ambiguous daruma scope `{scope}`; use `scope_path` or `project_id`")
            }
        }
    }

    /// Resolve by filesystem path using the longest configured path prefix.
    pub fn project_for_path(&self, path: &str) -> anyhow::Result<Option<String>> {
        let path = self.resolve_path(path)?;
        Ok(self
            .scopes
            .iter()
            .filter_map(|(key, project_id)| {
                let scope_path = normalize_path(key);
                path_is_inside(&path, &scope_path).then_some((scope_path.len(), project_id))
            })
            .max_by_key(|(len, _)| *len)
            .map(|(_, project_id)| project_id.clone()))
    }

    /// Resolve the current workspace key. If the key is a parent directory of
    /// multiple configured scopes, return a friendly ambiguity error instead
    /// of silently falling back to unscoped project-less behavior.
    pub fn inferred_project(&self) -> anyhow::Result<Option<String>> {
        if let Some(project_id) = env_project() {
            return Ok(Some(project_id));
        }
        let Some(key) = self.key.as_deref() else {
            return Ok(None);
        };

        let key = normalize_path(key);
        let nested = self
            .scopes
            .iter()
            .filter(|(scope, _)| path_is_inside(&normalize_path(scope), &key))
            .cloned()
            .collect::<Vec<_>>();
        if nested.len() > 1 {
            anyhow::bail!("{}", ambiguous_scope_message(&key, &nested));
        }
        if let Some(project_id) = self.project_for_path(&key)? {
            return Ok(Some(project_id));
        }
        Ok(nested.into_iter().next().map(|(_, project_id)| project_id))
    }

    /// Compute the scope path a `daruma_project_use` binding should be stored
    /// under: an explicit `scope_path` (resolved against the workspace key
    /// when relative), else the workspace-derived scope.
    pub fn scope_for_binding(&self, scope_path: Option<&str>) -> anyhow::Result<String> {
        match scope_path {
            Some(path) => self.resolve_path(path),
            None => self.scope_for_current_workspace(),
        }
    }

    fn resolve_path(&self, path: &str) -> anyhow::Result<String> {
        let path = path.trim();
        if Path::new(path).is_absolute() {
            return Ok(normalize_path(path));
        }
        let Some(key) = self.key.as_deref() else {
            anyhow::bail!(
                "relative `scope_path` `{path}` needs a local workspace; \
                 pass an absolute path"
            );
        };
        Ok(normalize_path(
            &Path::new(key).join(path).to_string_lossy(),
        ))
    }

    fn scope_for_current_workspace(&self) -> anyhow::Result<String> {
        let Some(key) = self.key.as_deref() else {
            anyhow::bail!("no local workspace for this MCP session; pass `scope_path`");
        };
        let key = normalize_path(key);
        let child_scopes = self
            .scopes
            .iter()
            .filter(|(scope, _)| path_is_inside(&normalize_path(scope), &key))
            .collect::<Vec<_>>();
        if child_scopes.len() > 1 {
            anyhow::bail!(
                "current workspace `{key}` contains multiple daruma scopes; pass `scope_path`"
            );
        }
        let containing_scope = self
            .scopes
            .iter()
            .filter(|(scope, _)| path_is_inside(&key, &normalize_path(scope)))
            .max_by_key(|(scope, _)| normalize_path(scope).len());
        if let Some((scope, _)) = containing_scope {
            return Ok(normalize_path(scope));
        }
        Ok(key)
    }
}

/// Upsert (or clear with `project_id: None`) a binding via
/// `PUT /v1/repo-scopes`.
pub async fn bind(
    client: &ApiClient,
    scope_path: &str,
    project_id: Option<&str>,
) -> anyhow::Result<Value> {
    client
        .put_json(
            "/v1/repo-scopes",
            json!({ "scope_path": scope_path, "project_id": project_id }),
        )
        .await
}

/// Best-effort current default project (env override, then server bindings).
pub async fn default_project(client: &ApiClient) -> Option<String> {
    if let Some(project_id) = env_project() {
        return Some(project_id);
    }
    ScopeView::fetch_or_empty(client)
        .await
        .inferred_project()
        .ok()
        .flatten()
}

/// One-time migration of the legacy file store: push every
/// `workspaces.json` binding to the server, then rename the file to
/// `workspaces.json.migrated`. Returns the number of migrated bindings;
/// `0` when there is no file. On any PUT failure the file is left in
/// place so the next startup retries.
pub async fn migrate_workspaces_file(client: &ApiClient) -> anyhow::Result<usize> {
    let path = paths::workspaces_file();
    let _ = paths::migrate_legacy_workspaces(&path);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(0);
    };

    #[derive(Default, Deserialize)]
    struct LegacyFile {
        #[serde(default)]
        workspaces: BTreeMap<String, String>,
    }
    let legacy: LegacyFile = serde_json::from_str(&text)?;

    let mut migrated = 0;
    for (scope, project_id) in &legacy.workspaces {
        if project_id.is_empty() {
            continue;
        }
        if let Err(e) = bind(client, &normalize_path(scope), Some(project_id)).await {
            // ponytail: string-match — ApiClient flattens the HTTP status
            // into the error message. A 404 is a stale binding to a deleted
            // project: drop it instead of blocking the migration forever.
            if e.to_string().contains("HTTP 404") {
                continue;
            }
            return Err(e);
        }
        migrated += 1;
    }
    std::fs::rename(&path, path.with_extension("json.migrated"))?;
    Ok(migrated)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::env_lock;

    fn view(key: &str, scopes: &[(&str, &str)]) -> ScopeView {
        ScopeView::new(
            Some(key.to_string()),
            scopes
                .iter()
                .map(|(scope, project_id)| (scope.to_string(), project_id.to_string()))
                .collect(),
        )
    }

    #[test]
    fn project_for_path_uses_longest_matching_scope() {
        let _guard = env_lock();
        std::env::remove_var("DARUMA_PROJECT_ID");
        let ws = view(
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
            ws.project_for_path("/tmp/daruma-public-test/projects/daruma-secondary/crates/api")
                .unwrap(),
            Some("secondary".to_string())
        );
    }

    #[test]
    fn inferred_project_selects_single_repo_scope() {
        let _guard = env_lock();
        std::env::remove_var("DARUMA_PROJECT_ID");
        let ws = view(
            "/tmp/daruma-public-test/projects/daruma/crates/mcp",
            &[("/tmp/daruma-public-test/projects/daruma", "oss")],
        );

        assert_eq!(ws.inferred_project().unwrap(), Some("oss".to_string()));
    }

    #[test]
    fn inferred_project_errors_for_ambiguous_parent_scope_even_with_parent_mapping() {
        let _guard = env_lock();
        std::env::remove_var("DARUMA_PROJECT_ID");
        let ws = view(
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
        let ws = view(
            "/tmp/daruma-public-test/projects",
            &[(
                "/tmp/daruma-public-test/projects/daruma-secondary",
                "secondary",
            )],
        );

        assert_eq!(
            ws.project_for_path("daruma-secondary/crates/api").unwrap(),
            Some("secondary".to_string())
        );
    }

    #[test]
    fn scope_for_binding_requires_scope_path_for_multi_repo_parent() {
        let _guard = env_lock();
        std::env::remove_var("DARUMA_PROJECT_ID");
        let ws = view(
            "/tmp/daruma-public-test/projects",
            &[
                ("/tmp/daruma-public-test/projects/daruma", "oss"),
                (
                    "/tmp/daruma-public-test/projects/daruma-secondary",
                    "secondary",
                ),
            ],
        );

        let err = ws.scope_for_binding(None).unwrap_err().to_string();
        assert!(err.contains("contains multiple daruma scopes"));

        let scope = ws.scope_for_binding(Some("daruma-secondary")).unwrap();
        assert_eq!(scope, "/tmp/daruma-public-test/projects/daruma-secondary");
    }

    #[test]
    fn scope_for_binding_updates_existing_scope_from_subdir() {
        let _guard = env_lock();
        std::env::remove_var("DARUMA_PROJECT_ID");
        let ws = view(
            "/tmp/daruma-public-test/projects/daruma/crates/mcp",
            &[("/tmp/daruma-public-test/projects/daruma", "old")],
        );

        let scope = ws.scope_for_binding(None).unwrap();
        assert_eq!(scope, "/tmp/daruma-public-test/projects/daruma");
    }

    #[test]
    fn server_mode_without_key_needs_absolute_scope_path() {
        let _guard = env_lock();
        std::env::remove_var("DARUMA_PROJECT_ID");
        let ws = ScopeView::new(None, vec![("/srv/repo".to_string(), "prj".to_string())]);

        // Absolute paths resolve against server-side bindings.
        assert_eq!(
            ws.project_for_path("/srv/repo/sub").unwrap(),
            Some("prj".to_string())
        );
        // No CWD → inference yields nothing instead of erroring.
        assert_eq!(ws.inferred_project().unwrap(), None);
        // Relative paths and keyless binding ask for scope_path.
        assert!(ws.project_for_path("sub").is_err());
        assert!(ws.scope_for_binding(None).is_err());
        assert_eq!(ws.scope_for_binding(Some("/srv/repo")).unwrap(), "/srv/repo");
    }
}
