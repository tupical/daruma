set dotenv-load := true
set shell := ["bash", "-uc"]

home := env_var_or_default("HOME", ".")
data_dir := env_var_or_default("TASKAGENT_DATA_DIR", home + "/.agents/taskagent/data")

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

server:
    mkdir -p "{{data_dir}}"
    CARGO_BUILD_RUSTC_WRAPPER="${CARGO_BUILD_RUSTC_WRAPPER:-}" TASKAGENT_DATA_DIR="{{data_dir}}" cargo run -p taskagent-server

mcp:
    CARGO_BUILD_RUSTC_WRAPPER="${CARGO_BUILD_RUSTC_WRAPPER:-}" cargo run -p taskagent-cli -- mcp

desktop:
    CARGO_BUILD_RUSTC_WRAPPER="${CARGO_BUILD_RUSTC_WRAPPER:-}" cargo run -p taskagent-desktop

# The browser UI now lives in the standalone `taskagent-web` repo (sibling
# checkout). Build/serve it there: `cd ../taskagent-web && trunk build|serve`.

docker-build:
    docker compose build server

docker-up:
    docker compose up -d server

docker-down:
    docker compose down
