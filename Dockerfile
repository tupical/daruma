# syntax=docker/dockerfile:1.7

# =========================================================================
# Build stage — compiles daruma-server (release).
# `daruma-desktop` (GPUI) is excluded; it builds on the host.
# =========================================================================
FROM rust:1.95-slim-bookworm AS builder

RUN sed -i -e 's|deb.debian.org/debian-security|security.debian.org/debian-security|g' -e 's|deb.debian.org/debian|http.us.debian.org/debian|g' /etc/apt/sources.list.d/debian.sources /etc/apt/sources.list 2>/dev/null || true \
 && apt-get update \
 && apt-get install -y --no-install-recommends \
        pkg-config \
        ca-certificates \
        libssl-dev \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY Cargo.toml rust-toolchain.toml ./
COPY crates ./crates
COPY apps ./apps

ARG SERVER_FEATURES=""

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/app/target \
    if [ -n "$SERVER_FEATURES" ]; then \
        cargo build --release -p daruma-server --features "$SERVER_FEATURES"; \
    else \
        cargo build --release -p daruma-server; \
    fi \
 && cargo build --release -p daruma-cli \
 && cp /app/target/release/daruma-server /usr/local/bin/daruma-server \
 && cp /app/target/release/daruma /usr/local/bin/daruma-linux

FROM rust:1.95-slim-bookworm AS mcp-windows-builder

RUN sed -i -e 's|deb.debian.org/debian-security|security.debian.org/debian-security|g' -e 's|deb.debian.org/debian|http.us.debian.org/debian|g' /etc/apt/sources.list.d/debian.sources /etc/apt/sources.list 2>/dev/null || true \
 && apt-get update \
 && apt-get install -y --no-install-recommends \
        pkg-config \
        ca-certificates \
        libssl-dev \
        gcc-mingw-w64-x86-64 \
 && rm -rf /var/lib/apt/lists/* \
 && rustup target add x86_64-pc-windows-gnu

WORKDIR /app
COPY Cargo.toml rust-toolchain.toml ./
COPY crates ./crates
COPY apps ./apps
ENV CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=x86_64-w64-mingw32-gcc

RUN rustup target add x86_64-pc-windows-gnu \
    && cargo build --release -p daruma-cli --target x86_64-pc-windows-gnu \
 && cp /app/target/x86_64-pc-windows-gnu/release/daruma.exe /usr/local/bin/daruma-windows.exe

# =========================================================================
# Runtime stage — minimal image for the production server.
# =========================================================================
FROM debian:bookworm-slim AS runtime

RUN sed -i -e 's|deb.debian.org/debian-security|security.debian.org/debian-security|g' -e 's|deb.debian.org/debian|http.us.debian.org/debian|g' /etc/apt/sources.list.d/debian.sources /etc/apt/sources.list 2>/dev/null || true \
 && apt-get update \
 && apt-get install -y --no-install-recommends \
        ca-certificates \
        libssl3 \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --create-home --home-dir /app --shell /usr/sbin/nologin daruma \
 && mkdir -p /app/data /app/bin \
 && chown -R daruma:daruma /app

WORKDIR /app
USER daruma

COPY --from=builder /usr/local/bin/daruma-server /usr/local/bin/daruma-server
COPY --from=builder /usr/local/bin/daruma-linux /app/bin/daruma-linux
COPY --from=mcp-windows-builder /usr/local/bin/daruma-windows.exe /app/bin/daruma-windows.exe

ENV RUST_LOG=info \
    DARUMA_DATA_DIR=/app/data \
    DARUMA_MCP_BIN_DIR=/app/bin

VOLUME ["/app/data"]
EXPOSE 8080

ENTRYPOINT ["/usr/local/bin/daruma-server"]
