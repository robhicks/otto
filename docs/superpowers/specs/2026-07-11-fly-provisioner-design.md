# FlyTarget — on-demand remote execution on Fly.io

**Status:** Design approved (2026-07-11), pre-implementation.
**Crate:** `otto-remote` (`crates/remote/`), plus a new in-repo container image under `deploy/fly/`.

## Problem

otto's session-handover protocol is complete and gated, but every non-loopback path assumes an
`otto serve --accept-promotions` receiver *already exists*. `VpsTarget` pushes to a running server;
`FirecrackerProvisioner` boots a *local* microVM (feature-gated, operator-supplied kernel/rootfs).
Nothing stands up a genuinely-remote machine from nothing.

This design adds **on-demand remote execution on Fly.io**: promoting a session provisions a fresh
Fly Machine running `otto serve`, the client reconnects to it, and the machine is destroyed on
demote/stop. Fly Machines are Firecracker microVMs behind a REST API with edge-terminated TLS,
which maps cleanly onto otto's remote seam.

## Decisions (locked)

| Decision | Choice | Rationale |
|---|---|---|
| Machine lifecycle | **Long-lived remote session** — created on promote, destroyed only on explicit demote/stop; survives the source disconnecting. | Matches a real remote coding session (minutes–hours). This is `VpsTarget`'s lifecycle shape (explicit async teardown, no drop-magic), **not** `MicrovmTarget`'s ephemeral/process-tied one. |
| Seam fit | **`FlyTarget: RemoteTarget`**, mirroring `VpsTarget`. | The `Provisioner`/`MicrovmTarget` trait disposes the machine when its guardian task drops — wrong for a machine that must outlive the promote RPC. No changes to the shared seam. |
| Reachability | **One Fly app per session** → `wss://<app>.fly.dev`. | `fly-force-instance-id` routing to a specific machine works from the public internet, but **browser-JS WebSockets cannot set custom headers** (otto's UI is browser-first — it already passes the token via `?token=` for this reason). A per-app hostname needs no header and serves both the browser UI and the CLI. |
| Token | **Fresh per session**, injected via the machine's `env`. | Per-app isolation makes minting cheap; blast radius of a leaked bearer is one ephemeral session. Injected via machine `env` (not a Fly app secret) to avoid a separate GraphQL round-trip; readable only by holders of the Fly API token. App-secret injection is a documented future hardening. |
| Gating / tests | **Always-compiled**, wiremock-tested in CI. | `FlyTarget` only does HTTP (reqwest is already a crate dep), so unlike Firecracker it has no host-specific code and needs no cargo feature — the full create/poll/delete flow is CI-testable against a mocked Fly API. |
| Image scope | **Included** — Dockerfile + entrypoint contract + `fly.toml` ship with this design. | The provisioner is untestable-in-reality without a bootable otto image; this makes the design one deployable, end-to-end deliverable. |
| Cost backstop | **Fly-native** — `autostop=suspend` + `auto_destroy`, plus explicit teardown. | Explicit teardown is the happy path; an idle orphan suspends (≈no compute bill) and a stopped orphan self-destroys. Empty-app GC and an active reaper are follow-ups. |

## Architecture

New file **`crates/remote/src/fly.rs`** (always compiled), three units:

### `FlyConfig` — plain data, read at the CLI edge only

Read from `OTTO_FLY_*` / `FLY_API_TOKEN` in `cmd_serve` (never inside the crate — mirrors how
`MicrovmConfig` is read from `OTTO_FC_*`). Carried as data in `PromoteMode::Fly`.

```
struct FlyConfig {
    api_token: String,        // FLY_API_TOKEN — the real secret
    org_slug: String,         // OTTO_FLY_ORG
    region: String,           // OTTO_FLY_REGION (e.g. "iad")
    image: String,            // OTTO_FLY_IMAGE (e.g. registry.fly.io/otto-serve:latest)
    vm_cpus: u32,             // OTTO_FLY_CPUS (default 1)
    vm_mem_mib: u32,          // OTTO_FLY_MEM_MIB (default 1024)
    app_prefix: String,       // OTTO_FLY_APP_PREFIX (default "otto-session")
    internal_port: u16,       // OTTO_FLY_PORT (default 8787) — otto serve's in-guest port
    boot_timeout: Duration,   // OTTO_FLY_BOOT_TIMEOUT_MS (default ~30s)
    api_base: String,         // default "https://api.machines.dev/v1"; overridable for wiremock
}
```

### `FlyApi` — the only thing that talks to Fly

A thin reqwest client (`Bearer api_token`, `api_base` injectable for tests) exposing exactly the
calls we need:

- `create_app(app_name, org_slug)`
- `ensure_public_ip(app_name)` — allocate shared IPv4 + IPv6 so `<app>.fly.dev` resolves.
  **Tracked risk:** this may require a Fly GraphQL `allocateIpAddress` mutation rather than a
  Machines-REST endpoint; `ensure_public_ip` encapsulates it (still `Bearer`-auth'd, still
  wiremockable). The exact endpoint is confirmed during planning; the design does not hinge on it.
- `create_machine(app_name, spec)` — see request body below.
- `wait_ready(app_name, boot_timeout)` — poll `https://<app>.fly.dev/` until any HTTP status
  (every route is gated → 401/404 means "serve is listening") or timeout.
- `delete_app(app_name)` — removes the machine + IPs in one call.

Every non-2xx bails with `HTTP {status}: {body}` (operator diagnostics), matching the existing
`push_promote_bundle` / `export_bundle` convention.

**`create_machine` request body (the core builder, unit-tested):**

```
{
  "config": {
    "image": "<image>",
    "auto_destroy": true,                          // machine-level: destroy when it stops
    "env": { "OTTO_TOKEN": "<minted>", "OTTO_PORT": "<internal_port>", "OTTO_ROOT": "/workspace" },
    "guest": { "cpus": <vm_cpus>, "memory_mb": <vm_mem_mib> },
    "services": [{
      "protocol": "tcp",
      "internal_port": <internal_port>,
      "autostop": "suspend",                       // service-level: suspend idle machine
      "autostart": true,
      "min_machines_running": 0,
      "ports": [{ "port": 443, "handlers": ["tls", "http"] }]
    }]
  },
  "region": "<region>"
}
```

(`auto_destroy` at `config` top level; `autostop`/`autostart`/`min_machines_running` per service —
this is the Fly Machines schema, verified against the API reference.)

### `FlyTarget` — the `RemoteTarget` impl

Holds a `FlyApi` + `FlyConfig`. Drives `promote()`. Provides an inherent `export()` for demote,
exactly like `VpsTarget`.

## Data flow

### `FlyTarget::provision(bundle) -> RemoteHandle`

1. Mint a fresh random token; compute `app_name = {app_prefix}-{short-random}` (lowercase, DNS-safe, collision-resistant).
2. `create_app(app_name, org_slug)`.
3. `ensure_public_ip(app_name)`.
4. `create_machine(app_name, spec)` with the body above (token injected via `env`).
5. `wait_ready(app_name, boot_timeout)`.
6. `push_promote_bundle("wss://{app_name}.fly.dev", token, bundle)` — the **existing shared helper**;
   its `http_base` already maps `wss→https`, so the `/promote` POST hits Fly's edge TLS correctly.
7. Return `RemoteHandle::new("wss://{app_name}.fly.dev", token)` — **no backing task** (like `VpsTarget`).

**Failure handling (no leaks / no orphan bill):** any error in steps 3–6 triggers a best-effort
`delete_app(app_name)` before bailing. This reproduces `MicrovmTarget`'s "abort the machine if the
push fails" guarantee — done explicitly, since there is no task to drop.

### `FlyTarget::teardown(handle) -> ()`

Parse `<app>` back out of the `wss://<app>.fly.dev` endpoint → `delete_app(app)`. Stateless (no
stored machine id). Invoked on the demote/stop path. A malformed/unparseable endpoint is an error,
not a silent no-op.

### `FlyTarget::export(session) -> PromoteBundle`

Delegates to the shared `export_bundle` (demote pull), identical to `VpsTarget::export`.

### Cost backstop layering

1. Explicit `teardown` (demote/stop) — happy path, deletes the app.
2. `autostop=suspend` — an idle machine suspends, stopping compute billing.
3. `auto_destroy` — a stopped orphan machine self-destroys.
4. Residue: an empty app after auto-destroy (apps are free) — cleaned by a documented
   `fly apps list`-based sweep. **Follow-up, not v1 code.**

## Seam wiring (additive)

- `lib.rs`: add `PromoteMode::Fly { config: FlyConfig }` to the existing enum.
- `main.rs` `cmd_serve`: add `--promote-fly`, mutually exclusive with the other `--promote-*`
  flags (reuse the existing exclusivity guard); read `OTTO_FLY_*` / `FLY_API_TOKEN` → `FlyConfig`.
- `serve.rs` handover: build `FlyTarget` for the `Fly` arm, parallel to the existing `Vps` /
  `Microvm` arms.
- Client reconnect after promote is unchanged — it already reconnects to the returned
  `endpoint` + `token`.

## Container image (`deploy/fly/`)

- **`Dockerfile`** — multi-stage: build `otto` (release) → slim runtime (debian-slim or
  distroless+libc) with the binary at `/usr/local/bin/otto`; include `git`/`ripgrep` for the served
  spine. **No sandbox backend (`bwrap`) in-container**, so `bash`/sandboxed tools stay unregistered
  on the guest — fail-closed, consistent with otto's existing "no backend → no bash" rule. Stated
  explicitly so it is a deliberate posture, not an oversight.
- **Entrypoint contract** — `CMD` runs `otto serve --accept-promotions --port $OTTO_PORT --root
  $OTTO_ROOT`, reading `OTTO_TOKEN` / `OTTO_PORT` / `OTTO_ROOT` from env (the vars `create_machine`
  injects). This is the Fly analog of Firecracker's `guest_cmdline` contract.
- **`fly.toml`** — a reference/base app config (image, internal_port, the 443 service). Documents
  intent; `FlyTarget` sets per-machine services via the API rather than relying on this file.
- **`README.md`** — operator steps: build → push to `registry.fly.io` → set `FLY_API_TOKEN` +
  `OTTO_FLY_*` → `otto serve --promote-fly`.

## Testing

- **Unit:** token/app-name generation (DNS-safe, unique); endpoint→app-name parsing (round-trips
  `wss://x.fly.dev`, rejects malformed); the `create_machine` request-body builder (env / services /
  autostop / auto_destroy / guest all correct); `FlyConfig` from-env parsing + defaults.
- **wiremock integration (CI — the coverage win):** point `api_base` at a mock Fly API and assert:
  - the full `provision` sequence (create-app → ensure-ip → create-machine → poll-ready → the
    `/promote` POST);
  - the **failure path** — a 500 on create-machine triggers `delete_app` and surfaces the error;
  - `teardown` issues the `DELETE`.
- **No real Fly calls in CI.** Real end-to-end (build image, `--promote-fly`, promote a live
  session, demote) is a documented manual smoke test — as Firecracker's real-VM test is out-of-CI.

## Out of scope (v1 / YAGNI)

Active reaper; multi-session-per-app; dedicated IPs; app-secret token injection; automated
orphan-empty-app GC. All noted as follow-ups.

## Files touched

- **New:** `crates/remote/src/fly.rs`; `deploy/fly/{Dockerfile,fly.toml,README.md}`.
- **Edited:** `crates/remote/src/lib.rs` (`PromoteMode::Fly`, `mod fly` + re-exports);
  `crates/engine/src/main.rs` (`--promote-fly`, `OTTO_FLY_*` parsing);
  `crates/engine/src/serve.rs` (handover `Fly` arm).
- **New dep:** a small RNG (e.g. `rand`) for token/app-name generation; `wiremock` as a dev-dep
  (already used elsewhere in the workspace).
