# otto Design — mcp-bash (sandboxed-shell migration)

**Status:** approved design (spec). Implementation plan to follow in `docs/superpowers/plans/`.
**Date:** 2026-06-17

## Goal

Migrate the sandboxed shell tool from the in-process `BashTool` to an `mcp-bash` rmcp stdio
server, **preserving every security guarantee**: the command runs in the OS sandbox
(`bwrap`/`sandbox-exec`), the gate classifies `bash` as `Ask` (resolved only via the
allow-list), and the tool is registered only when a sandbox backend exists. Fourth and final
MCP-axis server (`mcp-lsp` is v2).

## Context

`BashTool` (`crates/tools/src/bash.rs`) runs `{command, timeout_ms?}` via `build_argv(policy,
root, command)` (`crates/tools/src/sandbox.rs`) with a tokio timeout + `kill_on_drop`, returning
`{stdout, stderr, exit_code}`. `sandbox.rs` exposes `SandboxPolicy::{None, Os{allow_net}}`,
`os_sandbox_available()`, and `build_argv` (the pure argv builder, already unit-tested; `Os`
fails closed when `bwrap`/`sandbox-exec` is absent). `build_tool_registry` registers `BashTool`
**only when `os_sandbox_available()`**, pairing it with `AllowListAskResolver(["bash"])`; the
gate special-cases `tool == "bash"` → `Ask` (a shell can't be path-vetted). With no backend,
`bash` is absent and `Ask` stays denied (fail-closed).

## Invariants that MUST survive the migration

1. The command runs in `SandboxPolicy::Os` (never `None`) — fs read-only except root, net/pid/
   ipc isolated, minimal env, timeout kills the process tree.
2. The tool is named **`bash`**, so the gate's `bash → Ask` rule + `AllowListAskResolver(["bash"])`
   apply unchanged.
3. The tool is available **only when `os_sandbox_available()`** is true; otherwise absent
   (fail-closed). Defense in depth: `mcp-bash` itself hardcodes `Os`, so it errors rather than
   running unsandboxed even if misregistered.

## Decisions (locked during brainstorming)

1. **Extract `run_sandboxed` as the shared core.** Move the spawn + timeout + `kill_on_drop` +
   output-shaping logic from `BashTool::call` into `otto-tools` as
   `sandbox::run_sandboxed(policy, root, command, timeout) -> anyhow::Result<Value>`. Both the
   in-process `BashTool` and `mcp-bash` call it — one definition of the sandbox-run semantics.
2. **`mcp-bash` hardcodes `SandboxPolicy::Os`** (never `None`) → fails closed without a backend.
3. **Tool name stays `bash`** → the Ask-gate + allow-list + sandbox-only registration carry over.
4. **Prefer-with-fallback**: the engine registers in-process `BashTool` (when sandboxed) then
   overwrites `bash` with the `mcp-bash`-backed tool; on `mcp-bash` failure it falls back to the
   in-process one. Both run the same `run_sandboxed` core. The `mcp-bash` step is gated on
   `os_sandbox_available()` (mirroring the in-process rule).

## Architecture

### `otto-tools` refactor (`crates/tools/src/sandbox.rs` + `bash.rs`)

Add to `sandbox.rs`:

```rust
/// Run `command` under `policy` with `root` as the writable root, killed after `timeout`.
/// Returns `{ "stdout": .., "stderr": .., "exit_code": <i32|null> }`. The security-critical
/// spawn/timeout/kill-on-drop logic, shared by the in-process BashTool and the mcp-bash server.
pub async fn run_sandboxed(
    policy: &SandboxPolicy,
    root: &Path,
    command: &str,
    timeout: Duration,
) -> anyhow::Result<serde_json::Value>;
```

It builds argv via `build_argv`, spawns with stdin null / stdout+stderr piped / `kill_on_drop(true)`,
applies `tokio::time::timeout` (on timeout the child is killed via `kill_on_drop` and an error is
returned), and returns the JSON result. `BashTool::call` becomes a thin wrapper:
`run_sandboxed(&self.policy, &self.root, command, timeout)`. (`BashTool`'s existing tests, which
use `SandboxPolicy::None`, keep passing unchanged.)

### `crates/mcp-bash` (new binary)

`mcp-bash <root>`, mirrors `mcp-fs`. A `BashServer { root }` with one rmcp tool:

```
bash { command: String, timeout_ms?: u64 } -> { stdout, stderr, exit_code }
```

The handler calls `otto_tools::sandbox::run_sandboxed(&SandboxPolicy::Os { allow_net: false },
&root, &command, timeout)` — `Os` is hardcoded; there is no path to `None`. Default timeout
matches `BashTool` (read the current default and reuse it). Deps: `rmcp` (same version/features
as `mcp-fs`), `otto-tools` (for `SandboxPolicy`/`run_sandboxed`), `tokio`, `serde`/`serde_json`/
`schemars`/`anyhow`.

### Engine wiring (`crates/engine`)

`connect_bash(bin, root)` (mirrors `connect_fs`/`connect_grep`/`connect_git`), re-exported as
`mcp_connect_bash`. In `build_tools_preferring_mcp`, add a bash step **only when
`os_sandbox_available()`**: try `mcp_connect_bash`, register its `bash` tool (overwriting the
in-process one), hold the connection; on failure log "mcp-bash unavailable; using in-process
sandboxed bash" and keep the in-process `BashTool`. The `AllowListAskResolver(["bash"])` set up
by `build_tool_registry` (when sandboxed) is unchanged and governs the MCP-backed `bash` the
same way (gate `Ask` → allow-list permits).

## Error handling & determinism

- No sandbox backend: the engine doesn't register `bash` at all (in-process or MCP), and the
  resolver is `DenyAsk` — `bash` stays denied. `mcp-bash` itself, if ever invoked without a
  backend, errors from `build_argv` (fail-closed).
- A command timeout returns a clean error (process tree killed via `kill_on_drop` + the pid
  namespace), not a hang.
- Determinism: the `run_sandboxed` core is tested with `SandboxPolicy::None` (no `bwrap` needed) —
  reproducible. The `Os`-path integration test self-skips when `os_sandbox_available()` is false.

## Testing

- **`run_sandboxed` core unit tests** (in `otto-tools`, `SandboxPolicy::None`): `echo` captures
  stdout + exit 0; a non-zero exit is reported; a `sleep` past a short timeout returns the
  timeout error (and the existing `BashTool` tests still pass through the wrapper).
- **`mcp-bash` unit test**: the server's core path returns the right shape for `echo` — run with
  `None` policy in the test (e.g. a test-only constructor or by calling `run_sandboxed` directly),
  OR gate on `os_sandbox_available()`. Keep it deterministic; do not weaken the production `Os`
  hardcode.
- **Engine integration test** (`escargot`-built `mcp-bash`, **`#[cfg_attr]`/guarded to skip when
  `!os_sandbox_available()`**): with a sandbox backend present, register `bash` via
  `mcp_connect_bash` in a `ToolRegistry` whose ask-resolver allows `bash`, then
  `registry.call("bash", {command:"echo hi"})` returns `{stdout contains "hi", exit_code:0}` over
  the real sandboxed MCP round-trip. When no backend, the test returns early (documented).
- **Gate invariant test** (no sandbox needed): a `ToolRegistry` with `DenyAsk` denies `bash`
  (proving the `Ask` floor), and one with `AllowListAskResolver(["bash"])` permits it — confirming
  the tool name `bash` still triggers the right gate path after the migration. (This may already
  be covered by existing gate tests; add an explicit one if not.)

**Implementation latitude:** rmcp wiring copies `mcp-fs`. The `run_sandboxed` extraction must
preserve the exact current spawn/timeout/kill-on-drop behavior (move, don't rewrite). The
integration test's skip mechanism is the implementer's choice (early-return on
`!os_sandbox_available()` is simplest).

## Out of scope (named, not silently dropped)

- **Unbounded stdout/stderr buffering** — a pre-existing `BashTool` deferral; carried, not
  introduced. (An output cap is a separate, later change applied to `run_sandboxed`.)
- **Per-call network policy** — `Os { allow_net: false }` stays fixed; a per-call toggle is not
  exposed.
- **Dropping the in-process `BashTool`** — kept as the sandboxed fallback until `mcp-bash` ships
  as a guaranteed sidecar.
- **`mcp-lsp`** — v2.
