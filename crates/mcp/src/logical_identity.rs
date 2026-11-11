//! Opt-in durable label bootstrap for the existing stdio/CLI HTTP-hop adapter.
//! No credentials or runtime ownership are persisted here.
use crate::ApiClient;
use anyhow::{bail, Context};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::Path,
    time::Duration,
};
use uuid::Uuid;

pub const HEADER: &str = "x-daruma-logical-agent-id";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Binding {
    version: u8,
    server: String,
    binding_id: Uuid,
    logical_agent_id: Option<Uuid>,
}

struct Lock(std::path::PathBuf);
impl Drop for Lock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn atomic_save(path: &Path, binding: &Binding) -> anyhow::Result<()> {
    let temporary = path.with_extension(format!("{}.tmp", Uuid::new_v4()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    file.write_all(&serde_json::to_vec(binding)?)?;
    file.sync_all()?;
    fs::rename(&temporary, path)?;
    #[cfg(unix)]
    fs::File::open(path.parent().context("binding has no parent")?)?.sync_all()?;
    Ok(())
}

/// Finish before dispatching any task/plan read, including initialize preload.
/// A crashed owner's lock is deliberately NOT guessed stale: fail incomplete
/// rather than race another writer or regenerate the server dedup key.
pub async fn bind(
    client: ApiClient,
    root: &Path,
    binding_name: &str,
    display_name: &str,
    reuse: Option<Uuid>,
) -> anyhow::Result<ApiClient> {
    if binding_name.is_empty()
        || binding_name.len() > 64
        || !binding_name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
    {
        bail!(
            "incomplete logical setup: binding name must be 1-64 ASCII letters, digits, '-' or '_'"
        );
    }
    fs::create_dir_all(root)
        .context("incomplete logical setup: cannot create identity directory")?;
    let path = root.join(format!("{binding_name}.json"));
    let lock_path = root.join(format!("{binding_name}.lock"));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(35);
    let _lock = loop {
        match OpenOptions::new().write(true).create_new(true).open(&lock_path) {
            Ok(mut file) => {
                writeln!(file, "{}", std::process::id())?;
                break Lock(lock_path.clone());
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && tokio::time::Instant::now() < deadline => tokio::time::sleep(Duration::from_millis(25)).await,
            Err(e) => return Err(e).context("incomplete logical setup: binding locked; inspect owner before removing a crash lock"),
        }
    };
    let mut binding = match fs::read(&path) {
        Ok(raw) => serde_json::from_slice::<Binding>(&raw).context(
            "incomplete logical setup: invalid identity state; do not regenerate its retry key",
        )?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Binding {
            version: 1,
            server: client.base_url().trim_end_matches('/').to_owned(),
            binding_id: Uuid::new_v4(),
            logical_agent_id: None,
        },
        Err(e) => return Err(e).context("incomplete logical setup: cannot read identity state"),
    };
    if binding.version != 1 || binding.server != client.base_url().trim_end_matches('/') {
        bail!(
            "incomplete logical setup: binding version/server mismatch; choose a separate binding"
        );
    }
    if let Some(id) = reuse {
        binding.logical_agent_id = Some(id);
    }
    // Persist retry key (and explicit selection intent) BEFORE any HTTP request.
    atomic_save(&path, &binding)
        .context("incomplete logical setup: cannot persist bootstrap key")?;
    let response = tokio::time::timeout(Duration::from_secs(30), async {
        match binding.logical_agent_id {
            Some(id) => client.get_json(&format!("/v1/cloud/agents/{id}")).await,
            None => {
                client
                    .post_json(
                        "/v1/cloud/agents",
                        json!({"binding_id": binding.binding_id, "name": display_name}),
                    )
                    .await
            }
        }
    })
    .await
    .context("incomplete logical setup: bootstrap timed out; retry with the saved key")?
    .context("incomplete logical setup: identity not verified; no task reads permitted")?;
    let id = response
        .get("id")
        .and_then(|v| v.as_str())
        .context("incomplete logical setup: server returned no ID")?
        .parse::<Uuid>()?;
    if binding
        .logical_agent_id
        .is_some_and(|selected| selected != id)
    {
        bail!("incomplete logical setup: server returned a different selected ID");
    }
    binding.logical_agent_id = Some(id);
    atomic_save(&path, &binding)
        .context("incomplete logical setup: cannot persist ID; retry with saved binding")?;
    Ok(client.with_logical_agent_id(id))
}
