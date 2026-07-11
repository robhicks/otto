# Multi-stage build of `otto` + its MCP server binaries into a slim runtime image for Fly Machines.
FROM rust:1.85-bookworm AS build
WORKDIR /src
COPY . .
# Build the engine plus the MCP server binaries that work without extra host deps: mcp-fs (fs.*),
# mcp-grep (in-process ripgrep-style search), mcp-git (git.* via the `git` binary). mcp-bash needs
# an OS sandbox backend (bwrap) and mcp-lsp needs language servers on PATH — neither is shipped in
# this slim image, so bash/lsp tools stay unavailable on the guest (documented in README).
RUN cargo build --release \
    -p otto-engine -p otto-mcp-fs -p otto-mcp-grep -p otto-mcp-git

FROM debian:bookworm-slim
# git backs the served spine's git.* tools (mcp-git shells out to it); ca-certificates for outbound
# TLS to LLM APIs. `gh` (GitHub CLI) is intentionally omitted: git.pr_open is unavailable on
# Fly-provisioned sessions as a result (see README Limitations); all other git.* ops work fine.
RUN apt-get update \
    && apt-get install -y --no-install-recommends git ca-certificates \
    && rm -rf /var/lib/apt/lists/*
# The engine + the MCP servers it spawns by name from PATH (mcp-fs/mcp-grep/mcp-git). Their default
# lookup names are `mcp-fs`/`mcp-grep`/`mcp-git` (overridable via OTTO_MCP_*_BIN).
COPY --from=build /src/target/release/otto /usr/local/bin/otto
COPY --from=build /src/target/release/mcp-fs /usr/local/bin/mcp-fs
COPY --from=build /src/target/release/mcp-grep /usr/local/bin/mcp-grep
COPY --from=build /src/target/release/mcp-git /usr/local/bin/mcp-git
# The session workspace root. create_machine injects OTTO_ROOT=/workspace.
RUN mkdir -p /workspace
# OTTO_HOST=0.0.0.0 is load-bearing: Fly's proxy reaches the machine over its network interface,
# not loopback, so otto serve must NOT bind 127.0.0.1 here (its safe local default). OTTO_TOKEN is
# injected per-machine by the provisioner.
ENV OTTO_PORT=8787 OTTO_ROOT=/workspace OTTO_HOST=0.0.0.0
# No OS sandbox backend (bwrap) is installed, so `bash`/sandboxed tools stay unregistered on the
# guest (fail-closed) — a deliberate posture consistent with otto's "no backend -> no bash" rule.
CMD ["sh", "-c", "otto serve --accept-promotions --port \"$OTTO_PORT\" --root \"$OTTO_ROOT\""]
