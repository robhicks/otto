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

# The web UI bundle. Its own stage: the Dioxus CLI is a heavy, separately-versioned toolchain, and
# keeping it out of the `build` stage means that stage's `cargo build` does not depend on `dx`
# being installed or working. This is NOT a caching win — both stages `COPY . .` over the same
# build context, so a UI-only change busts the `build` stage's copy layer (and its `cargo build`)
# exactly as much as an engine-only change does; there is no cache isolation between them. It is
# also not full failure isolation for the image as a whole: the final stage below still
# `COPY --from=webbuild`s the bundle, so a `dx build` failure still fails the overall image
# build — just not the engine `cargo build` itself.
#
# dx is pinned to the version this migration was verified against. build-web.sh runs the same
# four bundle-trust guards it runs locally, so this image cannot ship the unoptimized 2.16 MB
# wasm even if a future dx regresses wasm-opt.
FROM rust:1.85-bookworm AS webbuild
WORKDIR /src
COPY . .
RUN rustup target add wasm32-unknown-unknown \
    && cargo install dioxus-cli --version 0.7.9 --locked
RUN cd ui-dioxus && ./scripts/build-web.sh

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
# The web UI bundle, served by `otto serve --ui-dir` (see CMD). Built in the `webbuild` stage.
# SECURITY: this directory must contain only build output. The static route is unauthenticated
# by design (a browser fetches index.html and the wasm before it has a token), and ServeDir does
# not consult otto's sensitive-path floor — so OTTO_UI_DIR must never be pointed at /workspace.
COPY --from=webbuild /src/ui-dioxus/target/dx/otto-desktop/release/web/public /usr/local/share/otto/ui
# The session workspace root. create_machine injects OTTO_ROOT=/workspace.
RUN mkdir -p /workspace
# OTTO_HOST=0.0.0.0 is load-bearing: Fly's proxy reaches the machine over its network interface,
# not loopback, so otto serve must NOT bind 127.0.0.1 here (its safe local default). OTTO_TOKEN is
# injected per-machine by the provisioner.
ENV OTTO_PORT=8787 OTTO_ROOT=/workspace OTTO_HOST=0.0.0.0 OTTO_UI_DIR=/usr/local/share/otto/ui
# No OS sandbox backend (bwrap) is installed, so `bash`/sandboxed tools stay unregistered on the
# guest (fail-closed) — a deliberate posture consistent with otto's "no backend -> no bash" rule.
CMD ["sh", "-c", "otto serve --accept-promotions --port \"$OTTO_PORT\" --root \"$OTTO_ROOT\" --ui-dir \"$OTTO_UI_DIR\""]
