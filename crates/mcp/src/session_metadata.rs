//! Canonical agent-session metadata for IDE/MCP clients.
//!
//! Agents should call `daruma_session_start` with a `metadata` object so
//! tasks and comments can be traced back to a client chat / transcript.

use serde_json::{json, Map, Value};

/// Recommended keys written into `AgentSession.metadata`.
pub const KEY_CLIENT: &str = "client";
pub const KEY_MODEL: &str = "model";
pub const KEY_WORKSPACE_PATH: &str = "workspace_path";
pub const KEY_CHAT_ID: &str = "chat_id";
pub const KEY_TRANSCRIPT_PATH: &str = "transcript_path";
pub const KEY_HOST: &str = "host";
pub const KEY_GIT_WORK_CONTEXT: &str = "git_work_context";

/// Collect on the local client only. Hosted MCP must never inspect its own
/// checkout on behalf of a remote caller. Git failures mean unknown context.
pub fn git_work_context(path: &std::path::Path, merge_request_id: Option<&str>) -> Option<Value> {
    let git = |args: &[&str]| -> Option<String> {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(path)
            .args(args)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let text = String::from_utf8(output.stdout).ok()?;
        let value = text.trim_end_matches(['\r', '\n']);
        (!value.is_empty()).then(|| value.to_owned())
    };
    let worktree_path = git(&["rev-parse", "--show-toplevel"])?;
    // Git lists the main working tree first, even from a linked worktree.
    // NUL delimiters preserve spaces/newlines without Git's path quoting.
    let worktrees = git(&["worktree", "list", "--porcelain", "-z"]);
    let repo_root = worktrees
        .as_deref()
        .and_then(|text| text.split('\0').next())
        .and_then(|line| line.strip_prefix("worktree "));
    Some(json!({
        "repo_root": repo_root,
        "worktree_path": worktree_path,
        "head_sha": git(&["rev-parse", "--verify", "HEAD"]),
        "branch_ref": git(&["symbolic-ref", "--quiet", "HEAD"]),
        "merge_request_id": merge_request_id.map(str::trim).filter(|value| !value.is_empty()),
        "observed_at": daruma_shared::time::now().to_rfc3339(),
    }))
}

/// Merge caller `metadata` with env/workspace defaults (caller wins on conflict).
pub fn merge_defaults(mut metadata: Value) -> Value {
    let obj = metadata
        .as_object_mut()
        .map(|m| m.to_owned())
        .unwrap_or_default();
    let mut merged = Map::new();

    for (k, v) in default_entries() {
        merged.insert(k, v);
    }
    for (k, v) in obj {
        merged.insert(k, v);
    }

    Value::Object(merged)
}

fn default_entries() -> Vec<(String, Value)> {
    let mut out = Vec::new();

    if let Ok(client) = std::env::var("DARUMA_CLIENT") {
        if !client.trim().is_empty() {
            out.push((KEY_CLIENT.into(), json!(client.trim())));
        }
    }
    if let Ok(model) = std::env::var("DARUMA_MODEL") {
        if !model.trim().is_empty() {
            out.push((KEY_MODEL.into(), json!(model.trim())));
        }
    }
    if let Ok(chat_id) = std::env::var("DARUMA_CHAT_ID") {
        if !chat_id.trim().is_empty() {
            out.push((KEY_CHAT_ID.into(), json!(chat_id.trim())));
        }
    }
    if let Ok(path) = std::env::var("DARUMA_TRANSCRIPT_PATH") {
        if !path.trim().is_empty() {
            out.push((KEY_TRANSCRIPT_PATH.into(), json!(path.trim())));
        }
    }
    if let Ok(ws) = std::env::var("DARUMA_WORKSPACE") {
        if !ws.trim().is_empty() {
            out.push((KEY_WORKSPACE_PATH.into(), json!(ws.trim())));
        }
    } else if let Ok(cwd) = std::env::current_dir() {
        out.push((
            KEY_WORKSPACE_PATH.into(),
            json!(cwd.to_string_lossy().to_string()),
        ));
    }
    if let Ok(host) = std::env::var("DARUMA_HOST") {
        if !host.trim().is_empty() {
            out.push((KEY_HOST.into(), json!(host.trim())));
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::env_lock;

    #[test]
    fn merge_defaults_preserves_caller_overrides() {
        let _guard = env_lock();
        std::env::set_var("DARUMA_CLIENT", "cursor");
        std::env::set_var("DARUMA_MODEL", "env-model");

        let merged = merge_defaults(json!({
            "client": "codex",
            "model": "caller-model",
            "chat_id": "chat-1"
        }));

        assert_eq!(merged["client"], "codex");
        assert_eq!(merged["model"], "caller-model");
        assert_eq!(merged["chat_id"], "chat-1");
        assert!(merged.get(KEY_WORKSPACE_PATH).is_some());
        assert!(
            merged.get(KEY_GIT_WORK_CONTEXT).is_none(),
            "shared/hosted defaults must not inspect server Git"
        );

        std::env::remove_var("DARUMA_CLIENT");
        std::env::remove_var("DARUMA_MODEL");
    }

    #[test]
    fn git_snapshot_covers_unborn_main_and_detached_linked_worktrees() {
        let base =
            std::env::temp_dir().join(format!("daruma-git-context-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&base).unwrap();
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(base.clone());
        let root = base.join("main tree ");
        let linked = base.join("linked tree");
        std::fs::create_dir(&root).unwrap();
        assert!(git_work_context(&root, None).is_none());
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(&root)
                .args([
                    "-c",
                    "user.name=Test",
                    "-c",
                    "user.email=test@example.invalid",
                    "-c",
                    "commit.gpgsign=false",
                    "-c",
                    "core.hooksPath=/dev/null",
                ])
                .args(args)
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "--initial-branch=main"]);
        let unborn = git_work_context(&root, None).unwrap();
        assert!(unborn["head_sha"].is_null());
        assert_eq!(unborn["branch_ref"], "refs/heads/main");
        git(&["commit", "--allow-empty", "-m", "initial"]);
        let main = git_work_context(&root, Some(" 42 ")).unwrap();
        assert_eq!(main["repo_root"], root.to_str().unwrap());
        assert_eq!(main["worktree_path"], root.to_str().unwrap());
        assert_eq!(main["merge_request_id"], "42");
        assert!(main["head_sha"].as_str().unwrap().len() >= 40);
        git(&[
            "worktree",
            "add",
            "--detach",
            linked.to_str().unwrap(),
            "HEAD",
        ]);
        let detached = git_work_context(&linked, None).unwrap();
        assert_eq!(detached["repo_root"], main["repo_root"]);
        assert_eq!(detached["worktree_path"], linked.to_str().unwrap());
        assert_eq!(detached["head_sha"], main["head_sha"]);
        assert!(detached["branch_ref"].is_null());
        assert!(detached["merge_request_id"].is_null());
        assert!(detached["observed_at"].is_string());
    }
}
