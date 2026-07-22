# Release Contract

Daruma OSS is the versioned open-core runtime used by external modules:
`daruma-web`, desktop, CLI, and future
integrations. A released OSS core must be consumable without a mutable sibling
checkout.

## Versioned Core

- Release tags use `daruma-vMAJOR.MINOR.PATCH`.
- The workspace package version, root package version, and release tag must
  describe the same core version.
- Published modules pin the OSS core by git tag first. Local development may
  override the dependency with `vendor/oss -> ../daruma`, but the override is
  not the production contract.
- `/v1/healthz` exposes `{status, version, core_version, api_version}` so apps
  can detect runtime drift.

## Stable Surfaces

The release promise covers:

- REST `/v1/*` response and request DTOs.
- WS `/v1/ws` with subprotocol `daruma.v1`.
- MCP tool names and JSON schemas.
- `EventEnvelope` and event payload schema.
- Public Rust crates used by external modules:
  `daruma-shared`, `daruma-domain`, `daruma-events`,
  `daruma-api-dto`, plus `daruma-core` only for embed apps.

Patch and minor releases may add fields, endpoints, channels, capabilities, or
MCP tools. They must not remove or rename existing stable fields, routes,
channels, events, or tool arguments.

## Release Checklist

1. Update the workspace version in `Cargo.toml`.
2. Update `CHANGELOG.md` with user-visible changes and compatibility notes.
3. Run `cargo fmt --all -- --check`, `cargo test --workspace`, and
   `cargo check --workspace`.
4. Verify `/v1/healthz` reports the expected `core_version` and `api_version`.
5. Build `daruma-web` against the release candidate core.
6. Tag the release as `daruma-vMAJOR.MINOR.PATCH`.
7. Update dependent app repositories to the new tag, unless they intentionally
   stay on an older compatible version.

## App Dependency Policy

Each app repo must record its OSS dependency in `module.toml`:

```toml
[core]
source       = "git"
repo         = "https://github.com/tupical/daruma"
version      = "0.1.0"
tag          = "daruma-v0.1.0"
dev_override = "vendor/oss -> ../daruma"
```

Apps may keep path dependencies during active local development, but release
branches must be auditable back to an immutable OSS tag. Hosted-specific features
must stay outside this repo and consume OSS only through the public runtime
surface or the explicitly public Rust crates.
