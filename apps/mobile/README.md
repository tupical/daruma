<!--
module:
  name:     daruma-mobile
  kind:     client
  status:   wip
  contract: /v1/*
  owner:    clients
See module.toml for the machine-readable manifest.
-->

# daruma-mobile

Mobile client module for Daruma (§3.4 W3.1). The long-term target is a
**Tauri 2** shell on iOS/Android; this scaffold ships a minimal Rust binary
that proves the `/v1/*` HTTP contract from a separate workspace member.

Today the binary only performs `GET /v1/tasks` and prints JSON to stdout.
No UI, no in-process embed — same transport boundary as
[`apps/cli/`](../cli/) and the standalone `daruma-web` repo.

## Build

```bash
cargo build -p daruma-mobile
# binary at target/debug/daruma-mobile (or target/release/…)
```

## Configure

```bash
export DARUMA_API_URL=http://localhost:8080
export DARUMA_TOKEN=ag_dev_xxxxxxxx
```

When `DARUMA_TOKEN` is unset, the request is sent without a bearer
header (works only when the server allows unauthenticated reads).

## Run

With `daruma-server` listening on `:8080`:

```bash
cargo run -p daruma-mobile
```

Example output: pretty-printed JSON array of tasks from `/v1/tasks`.

## Layout

```
apps/mobile/
├── Cargo.toml      # workspace member; `daruma-mobile` binary
├── module.toml     # capabilities + contract manifest
├── README.md
└── src/
    └── main.rs     # GET /v1/tasks probe (Tauri UI later)
```

See [docs/MODULES.md](../../docs/MODULES.md) for the module registry.
