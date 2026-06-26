# Local development data layout

All local server state lives under **`~/.agents/daruma/data/`** (override with
`DARUMA_DATA_DIR`). MCP client config stays in the parent directory.

```text
~/.agents/daruma/
  workspaces.json    # repo path → project id (MCP)
  credentials.json   # remote/self-host profiles (CLI)
  data/              # daruma-server (canonical)
    daruma.sqlite
    workspacegraph.sqlite
    bootstrap.token
```

`daruma-web` does not choose the database — only the `daruma-server`
process on `:8080` does.

## Defaults in code

| Component | `DARUMA_DATA_DIR` unset |
|-----------|----------------------------|
| `daruma-server` | `~/.agents/daruma/data` |
| `just server` | same (via `Justfile`) |
| `daruma` CLI bootstrap | same (`daruma_mcp::paths::data_dir`) |

Override only when you need an isolated copy (e.g. tests):

```bash
export DARUMA_DATA_DIR=/path/to/custom/data
```

Do **not** use ad-hoc paths such as `<repo>/data` or `/tmp/daruma-*` for
normal development — agents should always start the server with the canonical
directory above.

## Local web stack

```bash
# Terminal 1 — API (from daruma repo)
just server

# Terminal 2 — UI (from daruma-web)
NO_COLOR=false trunk serve
```

Open (after first server start created `bootstrap.token`):

```text
http://127.0.0.1:5174/web/?token=$(cat ~/.agents/daruma/data/bootstrap.token)
http://127.0.0.1:5174/workspaces
```

Or use `daruma-web/scripts/dev-stack.sh`.

## Agents (Cursor, Codex, …)

1. Start `daruma-server` **without** inventing a new data path.
2. Token: `~/.agents/daruma/data/bootstrap.token`
3. MCP may still use Remote via `DARUMA_API_URL` — that is a **different**
   database from local `daruma.sqlite`.

## Legacy locations

| Old path | Action |
|----------|--------|
| `<daruma-repo>/data/` | Copy into `~/.agents/daruma/data/` once, then stop using repo `data/` |
| `/tmp/daruma-web-local/` | Same — merge if needed, then delete |

```bash
mkdir -p ~/.agents/daruma/data
cp -an /path/to/old/data/. ~/.agents/daruma/data/   # no clobber (-n)
```

Restart the server (no `DARUMA_DATA_DIR` override required).

## Backup & restore (SQLite event log)

The event log is the source of truth — everything else (projections,
WorkspaceGraph) can be rebuilt from it. The server runs SQLite in **WAL
mode** (`journal_mode=WAL`, `synchronous=NORMAL`, see
`crates/storage/src/db.rs`), which changes how you copy files safely.

### What to back up

```text
~/.agents/daruma/data/
  daruma.sqlite        # canonical event log + projections  ← back up
  daruma.sqlite-wal    # WAL (recent commits not yet checkpointed)
  daruma.sqlite-shm    # WAL shared-memory index (transient)
  workspacegraph.sqlite   # sidecar index — REBUILDABLE, optional
  bootstrap.token         # local admin token — back up if you rely on it
```

### Safe backup with the server running

A plain `cp daruma.sqlite` while the server is up is **not safe**: in
WAL mode the latest commits live in `-wal`, and copying the main file
mid-checkpoint can capture a torn state. Use SQLite's own backup:

```bash
sqlite3 ~/.agents/daruma/data/daruma.sqlite \
  ".backup '/backups/daruma-$(date +%F).sqlite'"
# or, equivalently:
sqlite3 ~/.agents/daruma/data/daruma.sqlite \
  "VACUUM INTO '/backups/daruma-$(date +%F).sqlite'"
```

Both produce a single consistent file (no `-wal`/`-shm` needed) and are
safe against a live writer. To fold the WAL into the main file first
(e.g. before an offline file-level copy):

```bash
sqlite3 ~/.agents/daruma/data/daruma.sqlite \
  "PRAGMA wal_checkpoint(TRUNCATE);"
```

### Cold backup (server stopped)

Stop `daruma-server`, then copy `daruma.sqlite` **together with**
`daruma.sqlite-wal` if it exists (or checkpoint first as above).
Never ship a main file with a stale `-wal` from a different point in time.

### Restore

1. Stop the server.
2. Replace `daruma.sqlite` with the backup file.
3. Delete any leftover `daruma.sqlite-wal` / `daruma.sqlite-shm`
   (they belong to the old database generation).
4. Optionally delete `workspacegraph.sqlite` — the sidecar index is
   re-derived from the event log on the next start/reindex.
5. Start the server.

### Self-host in a repo

If you point `DARUMA_DATA_DIR` inside a working copy, gitignore the
data files:

```gitignore
*.sqlite
*.sqlite-wal
*.sqlite-shm
bootstrap.token
```

### Future work

A `daruma export` CLI (portable JSON event-log dump) is tracked in the
roadmap; until then the SQLite-level backup above is the supported path.
