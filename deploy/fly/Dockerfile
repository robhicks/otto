# Multi-stage build of the `otto` binary into a slim runtime image for Fly Machines.
FROM rust:1.85-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release -p otto-engine

FROM debian:bookworm-slim
# git + ripgrep back the served spine's git/grep tools; ca-certificates for outbound TLS to LLM APIs.
RUN apt-get update \
    && apt-get install -y --no-install-recommends git ripgrep ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/otto /usr/local/bin/otto
# The session workspace root. create_machine injects OTTO_ROOT=/workspace.
RUN mkdir -p /workspace
ENV OTTO_PORT=8787 OTTO_ROOT=/workspace
# No OS sandbox backend (bwrap) is installed, so `bash`/sandboxed tools stay unregistered on the
# guest (fail-closed) — a deliberate posture consistent with otto's "no backend -> no bash" rule.
# OTTO_TOKEN is injected per-machine by the provisioner.
CMD ["sh", "-c", "otto serve --accept-promotions --port \"$OTTO_PORT\" --root \"$OTTO_ROOT\""]
