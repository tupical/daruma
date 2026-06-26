set dotenv-load := true
set shell := ["bash", "-uc"]

home := env_var_or_default("HOME", ".")
data_dir := env_var_or_default("DARUMA_DATA_DIR", home + "/.agents/daruma/data")

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
    CARGO_BUILD_RUSTC_WRAPPER="${CARGO_BUILD_RUSTC_WRAPPER:-}" DARUMA_DATA_DIR="{{data_dir}}" cargo run -p daruma-server

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
