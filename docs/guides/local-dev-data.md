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

Stop `daruma-server` (and every other process that has the database open),
then copy `daruma.sqlite` **together with** `daruma.sqlite-wal` if it exists
— the pair is one backup. After a crash or kill the WAL may still hold
committed transactions that are not in the main file yet; a main file
without its WAL is an incomplete backup. Alternatively, checkpoint first as
above: after `wal_checkpoint(TRUNCATE)` the main file alone is complete.
Never ship a main file with a `-wal` from a different point in time.
`daruma.sqlite-shm` is transient and does not need to be copied.

### Restore

How you restore depends on what the backup is.

**Self-contained copy** (`.backup`, `VACUUM INTO`, or a main file copied
after `wal_checkpoint(TRUNCATE)`) — a single file:

1. Stop `daruma-server` and every other process using the database.
2. Delete the old `daruma.sqlite`, `daruma.sqlite-wal` and
   `daruma.sqlite-shm` in the data directory — the leftover `-wal`/`-shm`
   belong to the old database generation and must not be applied to the
   restored file.
3. Copy the backup file in as `daruma.sqlite`.
4. Start the server.

**Cold pair** (main file + `-wal` copied with the server stopped):

1. Stop `daruma-server` and every other process using the database.
2. Delete the old `daruma.sqlite`, `daruma.sqlite-wal` and
   `daruma.sqlite-shm` in the data directory.
3. Copy **both** backup files in, keeping the names paired:
   `daruma.sqlite` and `daruma.sqlite-wal`. Do this before anything opens
   the database — opening the main file without its WAL loses the
   transactions that live only in the WAL.
4. Do **not** delete the restored `daruma.sqlite-wal`: SQLite replays it on
   the first open. Do not copy a `-shm`; SQLite rebuilds it.
5. Start the server.

```bash
# cold pair restore, server stopped
cd ~/.agents/daruma/data
rm -f daruma.sqlite daruma.sqlite-wal daruma.sqlite-shm
cp /backups/cold/daruma.sqlite /backups/cold/daruma.sqlite-wal .
```

You do not need to touch `workspacegraph.sqlite`. The sidecar index
remembers the sequence number and event id it last applied; on start the
server compares them with the restored event log. If the index is ahead of
the log, or the event at that sequence is a different one (the restored log
was written further after the backup), the server logs
`workspace graph is ahead of or diverged from the event log; rebuilding`,
clears the index and replays it from the start. Deleting the file before
starting the server forces the same full rebuild and stays a safe manual fallback.

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

A `daruma export` CLI (portable JSON event-log dump) is not yet
implemented; until then the SQLite-level backup above is the supported path.
