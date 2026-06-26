//! Bridge `~/.agents/daruma/credentials.json` (plugin-v2 schema) into MCP env.
//!
//! Explicit `DARUMA_API_URL` / `DARUMA_TOKEN` / `DARUMA_WORKSPACE_ID` win.

use serde::Deserialize;
use std::path::PathBuf;

use crate::paths::agent_dir;

#[derive(Debug, Deserialize)]
struct CredentialsFile {
    active_profile: Option<String>,
    profiles: Option<std::collections::HashMap<String, Profile>>,
}

#[derive(Debug, Deserialize)]
struct Profile {
    #[allow(dead_code)]
    mode: String,
    server_url: String,
    token: Option<String>,
    workspace_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedRemoteAuth {
    pub api_url: String,
    pub token: String,
    pub workspace_id: Option<String>,
}

pub fn credentials_path() -> PathBuf {
    agent_dir().join("credentials.json")
}

/// Load active profile when env vars are not already set.
pub fn resolve_from_agent_dir() -> Option<ResolvedRemoteAuth> {
    let path = credentials_path();
    let raw = std::fs::read_to_string(&path).ok()?;
    let doc: CredentialsFile = serde_json::from_str(&raw).ok()?;
    let name = doc.active_profile.as_deref()?;
    let profile = doc.profiles.as_ref()?.get(name)?;
    let token = profile.token.as_deref()?.trim();
    if token.is_empty() {
        return None;
    }
    let api_url = profile.server_url.trim().trim_end_matches('/').to_string();
    Some(ResolvedRemoteAuth {
        api_url,
        token: token.to_string(),
        workspace_id: profile
            .workspace_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::ENV_AGENT_DIR;
    use crate::test_support::env_lock;

    #[test]
    fn resolves_active_remote_profile() {
        let _guard = env_lock();
        let dir =
            std::env::temp_dir().join(format!("daruma-mcp-cred-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var(ENV_AGENT_DIR, &dir);
        let cred = dir.join("credentials.json");
        std::fs::write(
            &cred,
            r#"{
  "schema_version": 1,
  "active_profile": "remote-default",
  "profiles": {
    "remote-default": {
      "mode": "remote",
      "server_url": "https://remote.example",
      "token": "ta_pat_testtoken123456789012345678",
      "workspace_id": "019e5fc5-0000-7000-8000-000000000001"
    }
  }
}"#,
        )
        .unwrap();

        let resolved = resolve_from_agent_dir().unwrap();
        assert_eq!(resolved.api_url, "https://remote.example");
        assert_eq!(resolved.token, "ta_pat_testtoken123456789012345678");
        assert_eq!(
            resolved.workspace_id.as_deref(),
            Some("019e5fc5-0000-7000-8000-000000000001")
        );
        std::env::remove_var(ENV_AGENT_DIR);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
