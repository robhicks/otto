# `microvm` RemoteTarget via a `Provisioner` seam

**Date:** 2026-06-23
**Status:** Design — pending implementation plan.
**Depends on:** the engine axis (`RemoteTarget`, `promote()`, `RemoteHandle`, `PromoteBundle`,
`PromoteConfig`/`PromoteMode` in `crates/remote`; `LoopbackTarget` in `crates/engine`), the
restore-over-the-wire RPC (`POST /promote` + `EngineService::accept_promotion`, shipped with
`vps`), and the handover dispatch (`handle_handover` in `crates/engine/src/serve.rs`).

## Why

The distribution axis moves a session onto another engine in two shapes today:

- **`LoopbackTarget`** boots a real second in-process engine and restores the bundle into it.
- **`VpsTarget`** pushes the bundle to an **already-running, operator-managed** `otto serve
  --accept-promotions` over `POST /promote`.

The residual `UnsupportedTarget` marks the one missing capability: **creating the machine**
(hypervisor / SSH / cloud-SDK VM provisioning). A `microvm` target — an ephemeral, per-session
microVM — is exactly that missing step **plus** the bundle-push `VpsTarget` already does.

The key realization: `VpsTarget` assumed the serve already exists. A `microvm` target only adds
"first, create a reachable serve." That step is the natural new seam. Abstracting "boot a reachable
`otto serve --accept-promotions` and return how to reach it" behind a **`Provisioner`** trait makes
the target a thin composition — `MicrovmTarget = Provisioner(boot a machine) + bundle-push` — and,
crucially, makes the target logic **fully CI-testable** against an in-process serve, the same lever
that gave `VpsTarget` real coverage without external infrastructure.

## Goal & non-goals

**Goal.** A `Provisioner` seam, a provisioner-generic `MicrovmTarget` that composes it with the
existing restore-push, a real (feature-gated) `FirecrackerProvisioner`, and an
`UnsupportedProvisioner` that is the single honest "no hypervisor in-tree" boundary. The seam is
proven end-to-end in CI against an in-process serve; the Firecracker boot path is real code whose
host-side orchestration is unit-tested, with the actual VM boot left as an external integration.

**Non-goals (explicitly out of scope):**

- **Demote-from-microvm.** Pulling a session back off an ephemeral guest is a follow-up; in microvm
  mode `DemoteToLocal` returns an honest "not supported (ephemeral)" — the same posture vps demote
  started with. Loopback/vps demote are unchanged.
- **The guest image.** Building/shipping the guest kernel (`vmlinux`) and rootfs (with the `otto`
  binary + an init that launches `otto serve`) is operator-supplied. We define the **env-var /
  cmdline contract** the guest must honor; we do not build the image.
- **A real-VM integration test.** The `firecracker` feature's `provision()` against an actual
  hypervisor cannot run in CI. Only its host-side pure logic is unit-tested.
- **Multi-session receivers.** Unchanged from `vps`: a receiver serve hosts one workspace at a time.
- **Networking provisioning.** Creating the tap device / bridge / routes on the host is an operator
  prerequisite (documented). The provisioner consumes a pre-created tap by name.

## The seam

### `Provisioner` (in `crates/remote`)

```rust
#[async_trait]
pub trait Provisioner: Send + Sync {
    /// Boot a reachable `otto serve --accept-promotions` and return how to reach it. Disposal
    /// rides the returned `task`: aborting/dropping it stops the in-process serve or kills the
    /// microVM (mirrors `RemoteHandle::with_task`).
    async fn provision(&self) -> anyhow::Result<ProvisionedMachine>;
}

pub struct ProvisionedMachine {
    pub endpoint: String,                  // ws://host:port (guest IP for a microVM)
    pub token: String,                     // bearer the booted serve requires (shared from source)
    pub task: tokio::task::JoinHandle<()>, // owns disposal
}
```

There is **no separate `teardown` on the trait** — disposal travels with the machine, on `task`.
This is deliberate: it reuses `RemoteHandle::with_task` unchanged and matches serve.rs's established
lifecycle, where a handover handle is retained in `state.remotes` and disposed on **drop** (via
`RemoteHandle::abort` → `task.abort()`), never through an explicit async `teardown` call. For an
in-process serve, `task` *is* the serve task (abort stops serving — exactly what `LoopbackTarget`
does today). For a microVM, `task` is a **guardian** that owns a `Drop`-cleanup guard; aborting it
unwinds the task, dropping the guard, which synchronously kills the `firecracker` child and releases
its tempdir/tap.

### `MicrovmTarget` (in `crates/remote`)

```rust
pub struct MicrovmTarget { provisioner: Arc<dyn Provisioner> }

#[async_trait]
impl RemoteTarget for MicrovmTarget {
    async fn provision(&self, bundle: &PromoteBundle) -> anyhow::Result<RemoteHandle> {
        let m = self.provisioner.provision().await?;
        push_promote_bundle(&m.endpoint, &m.token, bundle).await?;
        Ok(RemoteHandle::with_task(m.endpoint, m.token, m.task))
    }
    async fn teardown(&self, mut handle: RemoteHandle) -> anyhow::Result<()> {
        handle.abort();
        Ok(())
    }
}
```

`MicrovmTarget` is **provisioner-generic** — it works for any `Provisioner`; Firecracker is the v2
provisioner and an in-process serve is the test provisioner. The name follows the roadmap vocabulary;
the genericity is visible in the field type.

If `push_promote_bundle` fails after a machine was provisioned, `provision` returns `Err` and the
just-created `ProvisionedMachine` (held in the local `m`) is dropped, aborting its `task` — so a
microVM that booted but rejected the bundle is **not leaked**. `handle_handover` then sends `Error`
and caches nothing (fail-closed; nothing half-promoted from the client's view).

### Shared restore-push (refactor `VpsTarget`)

Factor `VpsTarget::provision`'s `POST /promote` body into a free function in `crates/remote`:

```rust
/// POST the serialized bundle to `{http_base(endpoint)}/promote` with `Bearer token`. On non-2xx,
/// bail with the receiver's status + body (operator diagnostics). Shared by VpsTarget + MicrovmTarget.
pub(crate) async fn push_promote_bundle(endpoint: &str, token: &str, bundle: &PromoteBundle)
    -> anyhow::Result<()>;
```

`VpsTarget::provision` becomes a one-line caller; the `ws→http` / `wss→https` mapping
(`http_base`) and the existing error-surfacing move into the helper unchanged. This is
behavior-preserving — existing `vps` tests must keep passing — and means `MicrovmTarget` reuses the
**exact** restore-push, including the sensitive-floor honesty on the receiver side.

## The boundary: `UnsupportedProvisioner` replaces `UnsupportedTarget`

`UnsupportedTarget` is **retired** — declaration, `RemoteTarget` impl, its unit test, and the
`crates/engine/src/lib.rs` re-export are removed (blast radius is confined to `crates/remote` plus
that one re-export). Its role moves down a layer to:

```rust
pub struct UnsupportedProvisioner;
#[async_trait]
impl Provisioner for UnsupportedProvisioner {
    async fn provision(&self) -> anyhow::Result<ProvisionedMachine> {
        anyhow::bail!("real microVM provisioning requires a hypervisor + kernel/rootfs; \
                       not available in-tree (build with --features firecracker and supply OTTO_FC_*)")
    }
}
```

The machine-provisioning boundary is now expressed **once**, at the provisioner layer, exactly where
the missing capability lives. `MicrovmTarget` over an `UnsupportedProvisioner` surfaces this error
through `provision`, so the wiring degrades honestly when built without the `firecracker` feature.

## `FirecrackerProvisioner` (feature-gated, never in CI)

Lives in `crates/remote` behind a `firecracker` cargo feature, **off by default** — default builds
and the CI determinism suite never compile it, and the crate's dependency graph is unchanged (it
needs only `std::process` + the already-present `reqwest` for readiness polling).

### Host-side orchestration (`provision`)

1. **Jail.** Create a per-machine tempdir (the Firecracker working dir / chroot).
2. **Config.** Write the Firecracker machine-config JSON:
   - `boot-source`: `kernel_image_path` = `config.kernel`; `boot_args` = the guest cmdline append
     (below).
   - `drives`: one rootfs drive (`is_root_device: true`) = `config.rootfs`.
   - `network-interfaces`: one interface bound to the pre-created host tap `config.tap` with the
     guest MAC; the guest IP is derived from `config` (static, operator-assigned).
   - `machine-config`: `vcpu_count` / `mem_size_mib` from `config` (sane defaults).
3. **Guest contract (cmdline append).** The shared bearer token, guest serve port, and workspace
   root are passed to the guest via the kernel cmdline (e.g.
   `otto.token=<tok> otto.port=<p> otto.root=/workspace`). The operator's rootfs init reads these
   and launches `otto serve --accept-promotions` bound to the guest interface. (This contract is the
   documented seam between us and the externally-built image.)
4. **Spawn.** Launch `config.fc_bin` (`firecracker`) with the config file and an API socket in the
   jail; capture the `Child`.
5. **Readiness.** Poll `http(s)://<guest-ip>:<port>/` until **any HTTP response** arrives or the
   `config.boot_timeout` elapses. Every real route is gated, so "TCP connect + any status (401/404
   included)" is a sound liveness predicate; a timeout kills the child and returns `Err`.
6. **Return.** `ProvisionedMachine { endpoint: ws://<guest-ip>:<port>, token, task }` where `task` is
   a guardian: it owns a `FirecrackerGuard { child, jail_dir }` whose `Drop` (sync) kills the child
   (SIGKILL the process), then removes the jail tempdir. The task parks until aborted; abort →
   guard `Drop` → VM and scratch are reclaimed. (Tap teardown is the operator's, since the tap is an
   operator prerequisite — documented; the guard does not delete a tap it did not create.)

### Config as plain data, read at the edge

To preserve the convention that `crates/remote` core logic reads no `OTTO_*` env, the Firecracker
parameters are plain data:

```rust
pub struct MicrovmConfig {     // in crates/remote, always compiled (no VM deps)
    pub kernel: PathBuf, pub rootfs: PathBuf, pub fc_bin: PathBuf,
    pub tap: String, pub guest_ip: String, pub port: u16,
    pub vcpus: u32, pub mem_mib: u32, pub boot_timeout: Duration,
}
```

`main.rs` reads `OTTO_FC_*` (like `build_router` reads its env) and constructs `MicrovmConfig`,
carried as `PromoteMode::Microvm { config }`. `handle_handover` builds the provisioner from that
data with **no env access in the handler**. Without the `firecracker` feature, `MicrovmConfig` still
exists (plain data) but `handle_handover` builds `UnsupportedProvisioner`, so `--promote-microvm`
fails honestly rather than failing to compile.

## Wiring & CLI

- **`PromoteMode::Microvm { config: MicrovmConfig }`** added to the enum in `crates/remote`.
- **`handle_handover`** (serve.rs) gains a `Microvm` arm:
  - `to_remote == true` → build `MicrovmTarget::new(provisioner)` where `provisioner` is
    `FirecrackerProvisioner::new(config)` under `--features firecracker`, else
    `UnsupportedProvisioner`. Then the existing `promote(...)` → retain handle → reply `Promoted`
    path runs unchanged (idempotency cache + retain-before-reply ordering carry over).
  - `to_remote == false` → reply `Error` ("demote-from-remote not supported in microvm mode
    (ephemeral)") and return. (Per non-goals.)
- **`otto serve --promote-microvm`** (flag) selects the mode; provisioner parameters come from
  `OTTO_FC_*`. Mutually exclusive with `--promote-loopback` and `--promote-vps` (clap conflict +
  explicit check, matching the existing pair).

## Data flow (promote to an ephemeral microVM)

```
UI (connected to SOURCE serve, started with --promote-microvm; OTTO_FC_* set)
  └─ Command::PromoteToRemote
        └─ handle_handover  (PromoteMode::Microvm)
              └─ promote(store, workspace, session, &MicrovmTarget{provisioner})
                    ├─ store.snapshot(session)   → SessionState
                    ├─ workspace.snapshot()      → WorkspaceSnapshot
                    └─ MicrovmTarget::provision(bundle)
                          ├─ provisioner.provision()         (boot firecracker, poll readiness)
                          │     → ProvisionedMachine{ ws://guest-ip:port, token, guardian task }
                          └─ push_promote_bundle(endpoint, token, bundle)
                                └─ POST {http}/promote  (Bearer token, JSON bundle)
                                       │
GUEST microVM serve (--accept-promotions) ◄────────┘
   /promote handler → service.accept_promotion(bundle)
        ├─ store.restore(session)        (409 if already present)
        └─ workspace.restore(snapshot)   (gated apply_edit; sensitive floor enforced)
   → 200 { session }
        └─ RemoteHandle::with_task(ws://guest-ip:port, token, guardian)
  └─ ServerMessage::Promoted{ session, endpoint } → UI
        └─ UI drops SOURCE socket, reconnects to GUEST (?last_seq=) → resumes
   (on drop of the retained handle: guardian aborts → Drop kills the VM + clears the jail)
```

## Error handling & security

- **Honest boundary.** Without `--features firecracker`, `--promote-microvm` resolves to
  `UnsupportedProvisioner` and `provision` returns a clear, actionable error — never a silent or
  half state.
- **No leaked VM on rejected bundle.** A `push_promote_bundle` failure drops the freshly-provisioned
  machine, aborting its guardian task → the VM is killed and the jail removed. The source session
  stays put and `handle_handover` caches nothing.
- **Sensitive-path floor preserved.** The push hits the guest's gated `POST /promote`; workspace
  restore goes through `LocalWorkspace::apply_edit`, so a bundle carrying `.env`/`.ssh`/`.git`/keys
  is refused on the guest — identical to `vps`.
- **TLS.** `wss://` guest endpoints map to `https://` via the shared `http_base` in
  `push_promote_bundle`; a hardened deployment runs the guest serve with TLS. Token-in-URL caveats
  for `/ws` are unchanged.
- **Shared trust domain.** The guest serve reuses the source bearer token (the same v1 convention as
  loopback/vps); source and guest share a trust domain.

## Testing (real coverage, no infra)

1. **Crux — seam end-to-end, in CI.** A `TestServeProvisioner` (test fixture in
   `crates/engine/tests/`) implements `otto_remote::Provisioner` by booting an in-process `serve
   --accept-promotions` on `127.0.0.1:0` (the same `serve::app` boot `LoopbackTarget` uses) and
   returning its serve task. Build `MicrovmTarget::new(provisioner)`; create a session with a turn +
   workspace files in store A; `promote()` it through the target; connect a WS client to the
   provisioned endpoint with `?last_seq=0` and assert the session resumes (event log replays) and the
   restored workspace files are listable via the guest's `/workspace`. This proves the entire seam —
   provision → push → restore → resume — without a hypervisor.
2. **Disposal rides the task.** After `MicrovmTarget::teardown` (or dropping the retained
   `RemoteHandle`), assert the provisioned in-process serve is unreachable — proving abort →
   `task.abort()` disposes the machine.
3. **`UnsupportedProvisioner` refuses.** `MicrovmTarget::new(UnsupportedProvisioner).provision(...)`
   returns `Err`; assert the source session is untouched.
4. **`push_promote_bundle` refactor is behavior-preserving.** Existing `vps` promote/round-trip and
   `http_base` tests keep passing unchanged.
5. **Firecracker host-side pure logic** (compiled only under `--features firecracker`): the
   machine-config JSON shape (boot-source/drives/network/machine-config fields), the guest cmdline
   assembly (`otto.token`/`otto.port`/`otto.root`), the readiness predicate (any HTTP status counts
   as ready; a timeout errors), and missing-prerequisite errors (`fc_bin`/`kernel`/`rootfs` absent).
   No real VM is started.
6. **`handle_handover` microvm dispatch.** `PromoteToRemote` in microvm mode (without the feature) →
   `Error` from `UnsupportedProvisioner`; `DemoteToLocal` → `Error` ("not supported in microvm
   mode"). With a `TestServeProvisioner` injected, `PromoteToRemote` → `Promoted{endpoint}` at the
   provisioned serve.
7. **Determinism suite untouched.** `--promote-microvm` is opt-in, `firecracker` is behind a default-
   off feature, and all default paths stay offline — `cargo test --workspace` with no env vars is
   unchanged.

## Files touched (anticipated)

- `crates/remote/src/lib.rs` — `Provisioner` trait + `ProvisionedMachine`; `MicrovmTarget`;
  `UnsupportedProvisioner` (and remove `UnsupportedTarget` + its test); `push_promote_bundle` helper
  (refactor `VpsTarget::provision` to call it); `PromoteMode::Microvm { config }` + `MicrovmConfig`.
- `crates/remote/src/firecracker.rs` (new, `#[cfg(feature = "firecracker")]`) —
  `FirecrackerProvisioner`, config-JSON builder, cmdline builder, readiness poll, `FirecrackerGuard`
  (Drop cleanup), and its pure-logic unit tests.
- `crates/remote/Cargo.toml` — `firecracker` feature; `MicrovmConfig` needs no new deps.
- `crates/engine/src/serve.rs` — `handle_handover` `Microvm` arm (promote + honest demote refusal).
- `crates/engine/src/lib.rs` — drop the `UnsupportedTarget` re-export; export the new public types.
- `crates/engine/src/main.rs` — `--promote-microvm` flag, `OTTO_FC_*` → `MicrovmConfig`,
  mutual-exclusion with `--promote-loopback`/`--promote-vps`.
- `crates/engine/tests/microvm.rs` (new) — `TestServeProvisioner` + the seam end-to-end tests.
- `docs/ARCHITECTURE.md` / roadmap — record the `Provisioner` seam as the new machine-provisioning
  boundary; `microvm` provisioner shipped (Firecracker feature-gated); `UnsupportedTarget` retired;
  demote-from-microvm is the next follow-up.
