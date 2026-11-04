set dotenv-load := true
set shell := ["bash", "-uc"]

default:
    @just --list

check:
    CARGO_BUILD_RUSTC_WRAPPER="${CARGO_BUILD_RUSTC_WRAPPER:-}" cargo check --workspace

test:
    CARGO_BUILD_RUSTC_WRAPPER="${CARGO_BUILD_RUSTC_WRAPPER:-}" cargo test --workspace

clippy:
    CARGO_BUILD_RUSTC_WRAPPER="${CARGO_BUILD_RUSTC_WRAPPER:-}" cargo clippy --workspace --all-targets -- -D warnings

fmt:
    CARGO_BUILD_RUSTC_WRAPPER="${CARGO_BUILD_RUSTC_WRAPPER:-}" cargo fmt --all

fmt-check:
    CARGO_BUILD_RUSTC_WRAPPER="${CARGO_BUILD_RUSTC_WRAPPER:-}" cargo fmt --all -- --check

# Enable the tracked git hooks (DCO auto sign-off via .githooks/). Run once per clone.
hooks:
    git config core.hooksPath .githooks
    @echo "git hooks enabled — commits now auto-add a Signed-off-by trailer"

# The data directory is resolved by daruma-server itself
# (`daruma_mcp::paths::data_dir`), the same way the CLI resolves it:
# DARUMA_DATA_DIR, else $DARUMA_AGENT_DIR/data, else ~/.agents/daruma/data.
# Do not compute a fallback here — it would drift from the server and CLI.
server:
    CARGO_BUILD_RUSTC_WRAPPER="${CARGO_BUILD_RUSTC_WRAPPER:-}" cargo run -p daruma-server

mcp:
    CARGO_BUILD_RUSTC_WRAPPER="${CARGO_BUILD_RUSTC_WRAPPER:-}" cargo run -p daruma-cli -- mcp

desktop:
    CARGO_BUILD_RUSTC_WRAPPER="${CARGO_BUILD_RUSTC_WRAPPER:-}" cargo run -p daruma-desktop

# The browser UI now lives in the standalone `daruma-web` repo (sibling
# checkout). Build/serve it there: `cd ../daruma-web && trunk build|serve`.

docker-build:
    docker compose build server

docker-up:
    docker compose up -d server

docker-down:
    docker compose down
