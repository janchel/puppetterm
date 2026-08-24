# puppetterm — self-hosted web deployment
#
# Multi-stage build:
#   web   → builds the Svelte frontend (static site)
#   rust  → builds the headless server (no Tauri/GTK deps needed thanks to
#           the workspace split: server only depends on puppetterm-core)
#   agent → cross-builds the static Go remote-agent binaries
#   run   → slim runtime with openssh-client; keys are synced in by the
#           entrypoint from a read-only mount, so the container never writes
#           to the host's ~/.ssh.

# ---- frontend -----------------------------------------------------------------
FROM node:22-bookworm-slim AS web
WORKDIR /src/client
COPY client/package.json client/package-lock.json ./
RUN npm ci
COPY client/ .
RUN npm run build


# ---- rust server ---------------------------------------------------------------
FROM rust:1-bookworm AS rust
WORKDIR /src

# Dependency layer: manifests first so Docker caches the heavy crate builds.
COPY Cargo.toml Cargo.lock* ./
COPY core/Cargo.toml core/Cargo.toml
COPY server/Cargo.toml server/Cargo.toml
# The workspace lists all members, so every manifest must be present even
# though only puppetterm-server gets compiled here.
COPY client/src-tauri/Cargo.toml client/src-tauri/Cargo.toml
# Empty stubs compile fine — cargo builds every declared dependency either way.
RUN mkdir -p core/src server/src client/src-tauri/src \
    && echo "" > core/src/lib.rs \
    && printf 'fn main() {}\n' > server/src/main.rs \
    && echo "// stub" > client/src-tauri/src/lib.rs \
    && cargo build --release -p puppetterm-server

# Real sources (only our crates rebuild; deps come from cache).
COPY core/ core/
COPY server/ server/
RUN touch core/src/lib.rs server/src/main.rs \
    && cargo build --release -p puppetterm-server


# ---- go remote agent -------------------------------------------------------------
FROM golang:1.26-bookworm AS agent
WORKDIR /src/agent
COPY agent/ .
RUN make cross


# ---- runtime ----------------------------------------------------------------------
FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        openssh-client \
    && rm -rf /var/lib/apt/lists/*

# Non-root runtime user.
RUN useradd --create-home --uid 1000 --shell /bin/bash pp

COPY --from=rust  /src/target/release/puppetterm-server /app/puppetterm-server
COPY --from=web   /src/client/build                     /app/web
COPY --from=agent /src/agent/bin/                        /app/agent/bin/
COPY installer/                                        /app/installer/
COPY docker/entrypoint.sh                              /app/entrypoint.sh

RUN chmod +x /app/entrypoint.sh /app/puppetterm-server \
    && chown -R pp:pp /app

ENV HOME=/home/pp \
    PUPPETTERM_BIND=0.0.0.0 \
    PUPPETTERM_PORT=8080 \
    PUPPETTERM_WEB_DIST=/app/web \
    PUPPETTERM_AGENT_DIR=/app/agent/bin

EXPOSE 8080
WORKDIR /home/pp
USER root
ENTRYPOINT ["/app/entrypoint.sh"]
