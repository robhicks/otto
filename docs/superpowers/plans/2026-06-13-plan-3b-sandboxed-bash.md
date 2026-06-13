# otto Plan 3b — Sandboxed `bash` Tool Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add an in-process `bash` tool that runs shell commands confined by an OS sandbox (bwrap on Linux, sandbox-exec on macOS), gated so it only runs when both the permission gate permits it AND a sandbox backend is available — giving the future Verifier a way to run builds/tests safely.

**Architecture:** Builds on the Plan 3a `Tool` seam. A new `SandboxPolicy` + `build_argv` (otto-tools) wraps a `sh -c "<command>"` invocation in `bwrap`/`sandbox-exec` confinement (filesystem writes limited to the workspace root, network off). A new `BashTool` spawns that argv via tokio, captures stdout/stderr/exit, and enforces a timeout with kill-on-drop. The `DefaultPermissionGate` classifies `bash` as `Ask` (stricter than fs tools), and the engine registers `bash` ONLY when an OS sandbox is available, pairing it with an `AllowListAskResolver` that permits the (now-confined) `bash`. With no sandbox backend, `bash` is simply absent and `Ask` stays denied — fail-closed.

**Tech Stack:** Rust (edition 2024), tokio (process + time features), async-trait, anyhow, serde_json, tempfile (dev). OS tools: `bwrap` (Linux), `sandbox-exec` (macOS).

---

## Context for the implementer (read once)

Established by Plans 1–3a (on `main`):
- `engine-core::tool`: `Tool` trait (`fn name(&self) -> &str`, `async fn call(&self, Value) -> anyhow::Result<Value>`), `Decision { Allow, Ask, Deny }`, `PermissionGate::evaluate(tool, args) -> Decision`, `AskResolver::resolve(tool, args) -> bool`, `DenyAsk`, `ToolRegistry` (gate-before-dispatch).
- `otto-tools`: `DefaultPermissionGate` (case-insensitive sensitive-path floor over `path`/`paths`/`glob` args), `FsReadTool`/`FsWriteTool`/`FsListTool` (in `fs.rs`), `gate.rs`, `lib.rs` module root.
- `otto-engine`: `build_tool_registry(workspace: Arc<dyn Workspace>) -> ToolRegistry` registers the fs tools with `DefaultPermissionGate` + `DenyAsk`. The CLI/`run_goal` thread the registry through.

**Conventions (carry forward):**
- Git hygiene: stay on branch `feat/plan-3b-bash-sandbox`. NEVER `git checkout <sha>` / detach HEAD. Only `git add` + `git commit` (no `--amend`). Commit `Cargo.lock` when it updates.
- No AI/Claude self-attribution in commits.
- Per-package gates then a final workspace gate. `clippy -D warnings` clean.
- TDD: failing test → minimal impl → green → commit. Scope test-only imports into the test module to keep clippy clean.

**Security note for reviewers:** the sandbox argv (Task 1) is the actual security boundary. The unit tests verify argv *construction*; the real confinement must be sanity-checked against real `bwrap`/`sandbox-exec` semantics during review. Tasks 1–3 get security-focused review.

---

## File Structure

```
crates/
├── tools/src/
│   ├── sandbox.rs   # NEW: SandboxPolicy, os_sandbox_available(), build_argv()
│   ├── bash.rs      # NEW: BashTool (spawn wrapped argv, capture, timeout, kill-on-drop)
│   ├── gate.rs      # MODIFY: classify tool "bash" as Ask
│   ├── lib.rs       # MODIFY: re-export sandbox + bash items
│   └── Cargo.toml   # MODIFY: tokio process/time features
├── engine-core/src/
│   ├── tool.rs      # MODIFY: add AllowListAskResolver
│   └── lib.rs       # MODIFY: re-export AllowListAskResolver
└── engine/src/
    ├── lib.rs       # MODIFY: build_tool_registry registers sandboxed bash conditionally
    └── main.rs      # MODIFY: pass root to build_tool_registry
```

---

## Task 1: `sandbox.rs` — policy + argv builder

**Files:**
- Create: `crates/tools/src/sandbox.rs`
- Modify: `crates/tools/src/lib.rs`

- [ ] **Step 1: Write sandbox.rs with argv-construction tests**

Create `crates/tools/src/sandbox.rs`:

```rust
//! Confinement for shell commands. `build_argv` wraps a `sh -c "<command>"` invocation in an
//! OS sandbox (bwrap on Linux, sandbox-exec on macOS) that limits filesystem writes to the
//! workspace root and disables network unless allowed. The argv this produces IS the security
//! boundary — `bwrap --ro-bind / /` mounts the whole filesystem read-only, then `--bind root
//! root` re-mounts only the workspace writable; `--unshare-net` removes network access.

use std::path::Path;

/// How to confine a shell command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxPolicy {
    /// Run directly with no OS confinement. The cwd is still the workspace root, but the
    /// command can touch anything the host process can. Requires explicit opt-in.
    None,
    /// OS sandbox: bwrap (Linux) / sandbox-exec (macOS). Filesystem writes confined to the
    /// workspace root; network disabled unless `allow_net`.
    Os { allow_net: bool },
}

/// Return true if the program `bin` is on PATH.
fn which(bin: &str) -> bool {
    std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {bin} >/dev/null 2>&1"))
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Is an OS sandbox backend available on this host?
pub fn os_sandbox_available() -> bool {
    if cfg!(target_os = "linux") {
        which("bwrap")
    } else if cfg!(target_os = "macos") {
        which("sandbox-exec")
    } else {
        false
    }
}

/// Build the `(program, args)` to spawn for running `command` under `policy`, confined to
/// `root`. For `Os`, errors (fail-closed) if the backend isn't available or the platform is
/// unsupported.
pub fn build_argv(
    policy: &SandboxPolicy,
    root: &Path,
    command: &str,
) -> anyhow::Result<(String, Vec<String>)> {
    let root_str = root.to_string_lossy().to_string();
    match policy {
        SandboxPolicy::None => Ok(("sh".to_string(), vec!["-c".to_string(), command.to_string()])),
        SandboxPolicy::Os { allow_net } => {
            if cfg!(target_os = "linux") {
                if !which("bwrap") {
                    anyhow::bail!("OS sandbox requested but 'bwrap' is not available on PATH");
                }
                let mut args = vec![
                    "--ro-bind".to_string(),
                    "/".to_string(),
                    "/".to_string(),
                    "--bind".to_string(),
                    root_str.clone(),
                    root_str.clone(),
                    "--dev".to_string(),
                    "/dev".to_string(),
                    "--proc".to_string(),
                    "/proc".to_string(),
                    "--chdir".to_string(),
                    root_str,
                    "--die-with-parent".to_string(),
                ];
                if !allow_net {
                    args.push("--unshare-net".to_string());
                }
                args.push("sh".to_string());
                args.push("-c".to_string());
                args.push(command.to_string());
                Ok(("bwrap".to_string(), args))
            } else if cfg!(target_os = "macos") {
                if !which("sandbox-exec") {
                    anyhow::bail!("OS sandbox requested but 'sandbox-exec' is not available");
                }
                let net = if *allow_net {
                    "(allow network*)"
                } else {
                    "(deny network*)"
                };
                let profile = format!(
                    "(version 1)(allow default)(deny file-write*)\
                     (allow file-write* (subpath \"{root_str}\"))\
                     (allow file-write* (subpath \"/dev\"))\
                     (allow file-write* (subpath \"/tmp\")){net}"
                );
                Ok((
                    "sandbox-exec".to_string(),
                    vec![
                        "-p".to_string(),
                        profile,
                        "sh".to_string(),
                        "-c".to_string(),
                        command.to_string(),
                    ],
                ))
            } else {
                anyhow::bail!("OS sandbox is not supported on this platform")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn none_policy_is_plain_sh_c() {
        let (prog, args) = build_argv(&SandboxPolicy::None, &PathBuf::from("/work"), "echo hi").unwrap();
        assert_eq!(prog, "sh");
        assert_eq!(args, vec!["-c".to_string(), "echo hi".to_string()]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_os_policy_binds_root_and_unshares_net_when_disallowed() {
        // Only meaningful when bwrap exists; otherwise build_argv fails-closed (covered below).
        if !which("bwrap") {
            return;
        }
        let (prog, args) =
            build_argv(&SandboxPolicy::Os { allow_net: false }, &PathBuf::from("/work"), "ls").unwrap();
        assert_eq!(prog, "bwrap");
        // workspace is bind-mounted writable:
        assert!(args.windows(3).any(|w| w == ["--bind", "/work", "/work"]));
        // whole fs read-only:
        assert!(args.windows(3).any(|w| w == ["--ro-bind", "/", "/"]));
        // network removed:
        assert!(args.contains(&"--unshare-net".to_string()));
        // the actual command is the tail:
        assert_eq!(&args[args.len() - 3..], &["sh".to_string(), "-c".to_string(), "ls".to_string()]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_os_policy_keeps_net_when_allowed() {
        if !which("bwrap") {
            return;
        }
        let (_prog, args) =
            build_argv(&SandboxPolicy::Os { allow_net: true }, &PathBuf::from("/work"), "ls").unwrap();
        assert!(!args.contains(&"--unshare-net".to_string()));
    }
}
```

- [ ] **Step 2: Re-export from lib.rs**

Update `crates/tools/src/lib.rs` to add the module + re-exports (keep `fs`, `gate`):

```rust
//! otto in-process tools (behind `otto_engine_core::Tool`) and the default permission gate.

pub mod fs;
pub mod gate;
pub mod sandbox;

pub use fs::{FsListTool, FsReadTool, FsWriteTool};
pub use gate::DefaultPermissionGate;
pub use sandbox::{build_argv, os_sandbox_available, SandboxPolicy};
```

- [ ] **Step 3: Test**

Run: `cargo test -p otto-tools sandbox::` (the `none_policy_is_plain_sh_c` test always runs; the linux tests run/skip based on bwrap presence). Then `cargo test -p otto-tools` (all pass), `cargo clippy -p otto-tools --all-targets -- -D warnings` (clean), `cargo fmt -p otto-tools` (clean).

- [ ] **Step 4: Commit**

```bash
git add crates/tools/src/sandbox.rs crates/tools/src/lib.rs
git commit -m "feat(tools): SandboxPolicy + build_argv (bwrap/sandbox-exec confinement)"
```

---

## Task 2: `bash.rs` — the BashTool

**Files:**
- Modify: `crates/tools/Cargo.toml`
- Create: `crates/tools/src/bash.rs`
- Modify: `crates/tools/src/lib.rs`

- [ ] **Step 1: Add tokio process/time features**

In `crates/tools/Cargo.toml`, the lib now spawns processes. Add `tokio` to `[dependencies]` with the needed features (it was previously only a dev-dependency):

```toml
tokio = { workspace = true, features = ["process", "time", "io-util", "rt"] }
```

(Keep the existing `otto-engine-core`, `async-trait`, `anyhow`, `serde_json` deps. The `[dev-dependencies]` tokio with `macros`/`rt-multi-thread`/`fs` stays — cargo unions features.)

- [ ] **Step 2: Write bash.rs with tests**

Create `crates/tools/src/bash.rs`. The tool spawns the sandbox-wrapped argv with `kill_on_drop(true)`, so a timeout (which drops the wait future) kills the child.

```rust
//! `BashTool`: runs a shell command confined by a `SandboxPolicy`, with a timeout.

use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use otto_engine_core::tool::Tool;
use serde_json::{json, Value};

use crate::sandbox::{build_argv, SandboxPolicy};

const DEFAULT_TIMEOUT_MS: u64 = 30_000;

/// `bash` — args `{ "command": "<sh>", "timeout_ms": <n>? }` →
/// `{ "stdout": "...", "stderr": "...", "exit_code": <i32|null> }`.
pub struct BashTool {
    root: PathBuf,
    policy: SandboxPolicy,
}

impl BashTool {
    pub fn new(root: PathBuf, policy: SandboxPolicy) -> Self {
        Self { root, policy }
    }
}

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    async fn call(&self, args: Value) -> anyhow::Result<Value> {
        let command = args
            .get("command")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("bash requires a string 'command' arg"))?;
        let timeout = Duration::from_millis(
            args.get("timeout_ms").and_then(Value::as_u64).unwrap_or(DEFAULT_TIMEOUT_MS),
        );

        let (program, argv) = build_argv(&self.policy, &self.root, command)?;

        let mut cmd = tokio::process::Command::new(program);
        cmd.args(argv)
            .current_dir(&self.root)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);

        let child = cmd.spawn()?;
        let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
            // On timeout the wait_with_output future is dropped, and kill_on_drop kills the child.
            Err(_) => anyhow::bail!("bash command timed out after {} ms", timeout.as_millis()),
            Ok(result) => result?,
        };

        Ok(json!({
            "stdout": String::from_utf8_lossy(&output.stdout),
            "stderr": String::from_utf8_lossy(&output.stderr),
            "exit_code": output.status.code(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unsandboxed() -> BashTool {
        let dir = tempfile::tempdir().unwrap();
        // Leak the tempdir path into the tool; the dir is cleaned when the process exits.
        let root = dir.keep();
        BashTool::new(root, SandboxPolicy::None)
    }

    #[tokio::test]
    async fn runs_echo_and_captures_stdout() {
        let tool = unsandboxed();
        let out = tool.call(json!({"command": "echo hello"})).await.unwrap();
        assert!(out["stdout"].as_str().unwrap().contains("hello"));
        assert_eq!(out["exit_code"].as_i64().unwrap(), 0);
    }

    #[tokio::test]
    async fn captures_nonzero_exit_code() {
        let tool = unsandboxed();
        let out = tool.call(json!({"command": "exit 3"})).await.unwrap();
        assert_eq!(out["exit_code"].as_i64().unwrap(), 3);
    }

    #[tokio::test]
    async fn missing_command_arg_errors() {
        let tool = unsandboxed();
        let err = tool.call(json!({})).await.unwrap_err();
        assert!(err.to_string().contains("requires a string 'command'"));
    }

    #[tokio::test]
    async fn times_out_long_command() {
        let tool = unsandboxed();
        let err = tool
            .call(json!({"command": "sleep 5", "timeout_ms": 100}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("timed out"));
    }

    #[tokio::test]
    async fn sandboxed_runs_when_backend_available() {
        // Skips on hosts without an OS sandbox backend.
        if !crate::sandbox::os_sandbox_available() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let tool = BashTool::new(dir.path().to_path_buf(), SandboxPolicy::Os { allow_net: false });
        let out = tool.call(json!({"command": "echo sandboxed"})).await.unwrap();
        assert!(out["stdout"].as_str().unwrap().contains("sandboxed"));
        assert_eq!(out["exit_code"].as_i64().unwrap(), 0);
    }
}
```

Note on the test helper: `tempfile::TempDir::keep()` (formerly `into_path`) returns the `PathBuf` and prevents auto-deletion; the OS cleans `/tmp` later. If your tempfile version exposes `into_path()` instead of `keep()`, use that — the goal is to own a real directory path for the tool. Confirm which exists and use the available one.

- [ ] **Step 3: Re-export from lib.rs**

Update `crates/tools/src/lib.rs`:

```rust
//! otto in-process tools (behind `otto_engine_core::Tool`) and the default permission gate.

pub mod bash;
pub mod fs;
pub mod gate;
pub mod sandbox;

pub use bash::BashTool;
pub use fs::{FsListTool, FsReadTool, FsWriteTool};
pub use gate::DefaultPermissionGate;
pub use sandbox::{build_argv, os_sandbox_available, SandboxPolicy};
```

- [ ] **Step 4: Test**

Run: `cargo test -p otto-tools` (the 4 unsandboxed bash tests run everywhere; the sandboxed one runs/skips per host). `cargo clippy -p otto-tools --all-targets -- -D warnings` (clean), `cargo fmt -p otto-tools` (clean), `cargo test --workspace` (all pass). Commit Cargo.lock if changed.

- [ ] **Step 5: Commit**

```bash
git add crates/tools Cargo.lock
git commit -m "feat(tools): BashTool — sandboxed shell exec with timeout and kill-on-drop"
```

---

## Task 3: Gate `bash` as `Ask`; add `AllowListAskResolver`

**Files:**
- Modify: `crates/tools/src/gate.rs`
- Modify: `crates/engine-core/src/tool.rs`
- Modify: `crates/engine-core/src/lib.rs`

- [ ] **Step 1: Classify `bash` as `Ask` in the gate**

In `crates/tools/src/gate.rs`, in `DefaultPermissionGate::evaluate`, add a `bash` rule BEFORE the sensitive-path loop (shell commands can't be statically path-checked, so they always require explicit approval):

```rust
impl PermissionGate for DefaultPermissionGate {
    fn evaluate(&self, tool: &str, args: &Value) -> Decision {
        // Shell exec can't be statically vetted by path — it always requires explicit approval.
        if tool == "bash" {
            return Decision::Ask;
        }
        for p in Self::candidate_paths(args) {
            if Self::is_sensitive(&p) {
                return Decision::Deny;
            }
        }
        Decision::Allow
    }
}
```

Add a test to the gate test module:

```rust
    #[test]
    fn bash_requires_ask() {
        let gate = DefaultPermissionGate::new();
        assert_eq!(gate.evaluate("bash", &json!({"command": "ls"})), Decision::Ask);
    }
```

- [ ] **Step 2: Add `AllowListAskResolver` to engine-core**

In `crates/engine-core/src/tool.rs`, add an allow-list resolver after the `DenyAsk` definition:

```rust
/// Resolves `Ask` to allow only for an explicit allow-list of tool names. Used by the engine
/// to permit a tool that is `Ask`-gated but otherwise confined (e.g. a sandboxed `bash`).
pub struct AllowListAskResolver {
    allowed: Vec<String>,
}

impl AllowListAskResolver {
    pub fn new(allowed: Vec<String>) -> Self {
        Self { allowed }
    }
}

impl AskResolver for AllowListAskResolver {
    fn resolve(&self, tool: &str, _args: &Value) -> bool {
        self.allowed.iter().any(|t| t == tool)
    }
}
```

Add tests to the existing `#[cfg(test)] mod tests` in `tool.rs`:

```rust
    #[test]
    fn allow_list_resolver_allows_listed_tool_only() {
        let r = AllowListAskResolver::new(vec!["bash".to_string()]);
        assert!(r.resolve("bash", &json!({})));
        assert!(!r.resolve("fs.write", &json!({})));
    }
```

- [ ] **Step 3: Re-export `AllowListAskResolver`**

In `crates/engine-core/src/lib.rs`, add `AllowListAskResolver` to the tool re-export line:

```rust
pub use tool::{
    AllowListAskResolver, AskResolver, Decision, DenyAsk, PermissionGate, Tool, ToolRegistry,
};
```

- [ ] **Step 4: Test**

Run: `cargo test -p otto-tools gate::` (the new `bash_requires_ask` passes), `cargo test -p otto-engine-core tool::` (the new resolver test passes). Then `cargo clippy -p otto-tools -p otto-engine-core --all-targets -- -D warnings` (clean), `cargo fmt -p otto-tools -p otto-engine-core` (clean), `cargo test --workspace` (all pass).

- [ ] **Step 5: Commit**

```bash
git add crates/tools/src/gate.rs crates/engine-core
git commit -m "feat(tools,engine-core): gate bash as Ask; add AllowListAskResolver"
```

---

## Task 4: Engine wiring — register sandboxed `bash` conditionally

**Files:**
- Modify: `crates/engine/src/lib.rs`
- Modify: `crates/engine/src/main.rs`
- Modify: `crates/engine/tests/turn.rs`

- [ ] **Step 1: `build_tool_registry` takes the root and conditionally registers bash**

In `crates/engine/src/lib.rs`, update the imports to add the bash/sandbox/resolver items:

```rust
use otto_engine_core::tool::{AllowListAskResolver, AskResolver, DenyAsk, ToolRegistry};
use otto_tools::{
    BashTool, DefaultPermissionGate, FsListTool, FsReadTool, FsWriteTool, SandboxPolicy,
    os_sandbox_available,
};
```

(Keep the existing `use otto_engine_core::traits::Workspace;` etc.) Replace `build_tool_registry` with a version that takes the workspace root and registers a sandboxed `bash` ONLY when an OS sandbox backend exists — pairing it with an allow-list resolver so the `Ask`-gated bash is permitted *because* it is confined. Without a sandbox backend, bash is absent and the resolver is `DenyAsk` (fail-closed):

```rust
/// Build the default tool registry. Always includes the sensitive-path-floor gate and the
/// in-process fs tools. A sandboxed `bash` tool is registered ONLY when an OS sandbox backend
/// (bwrap/sandbox-exec) is available; in that case the `Ask` verdict the gate gives `bash` is
/// resolved by an allow-list resolver (safe because the registered bash is OS-confined).
/// With no sandbox backend, `bash` is absent and the resolver denies all `Ask` (fail-closed).
pub fn build_tool_registry(workspace: Arc<dyn Workspace>, root: PathBuf) -> ToolRegistry {
    let sandboxed = os_sandbox_available();
    let ask: Arc<dyn AskResolver> = if sandboxed {
        Arc::new(AllowListAskResolver::new(vec!["bash".to_string()]))
    } else {
        Arc::new(DenyAsk)
    };

    let mut registry = ToolRegistry::new(Arc::new(DefaultPermissionGate::new()), ask);
    registry.register(Arc::new(FsReadTool::new(Arc::clone(&workspace))));
    registry.register(Arc::new(FsWriteTool::new(Arc::clone(&workspace))));
    registry.register(Arc::new(FsListTool::new(workspace)));

    if sandboxed {
        registry.register(Arc::new(BashTool::new(root, SandboxPolicy::Os { allow_net: false })));
    }

    registry
}
```

This requires `PathBuf` in scope — `crates/engine/src/lib.rs` may not import it. Add `use std::path::PathBuf;` to the imports if absent.

- [ ] **Step 2: Update the CLI call site**

In `crates/engine/src/main.rs`, `build_tool_registry` now takes a second arg (the root). The CLI already has `root: PathBuf` and builds `tools_workspace`. Update the call. Replace the `let tools = build_tool_registry(tools_workspace);` line with:

```rust
    let tools = build_tool_registry(tools_workspace, root.clone());
```

Wait — `root` was already moved into the second `LocalWorkspace::new(root)` in Plan 3a's main.rs. Adjust so `root` is still available: change the tools-workspace construction to clone, so `root` survives for `build_tool_registry`. The relevant block should read:

```rust
    let router = build_router();
    let workspace = LocalWorkspace::new(root.clone());
    let tools_workspace: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(root.clone()));
    let tools = build_tool_registry(tools_workspace, root);

    let (events, outcome) = run_goal(&goal, router.as_ref(), &workspace, &tools).await?;
```

(`root` is consumed by the final `build_tool_registry` call; the two earlier uses clone it.)

- [ ] **Step 3: Update the integration test**

In `crates/engine/tests/turn.rs`, `build_tool_registry` now takes the root. Update the call. The test builds `tools_workspace` over `dir.path()`; pass `dir.path().to_path_buf()` as the root:

```rust
    let tools_workspace: std::sync::Arc<dyn Workspace> =
        std::sync::Arc::new(LocalWorkspace::new(dir.path()));
    let tools = build_tool_registry(tools_workspace, dir.path().to_path_buf());
```

All existing assertions stay unchanged (the turn does not call bash; bash registration is environment-dependent and does not affect the deterministic turn).

- [ ] **Step 4: Full workspace test + CLI smoke**

Run: `cargo test --workspace` (all pass), `cargo clippy --workspace --all-targets -- -D warnings` (clean), `cargo fmt --all -- --check` (clean). Smoke: `mkdir -p /tmp/otto-p3b && cargo run -p otto-engine -- run "add a greeting" --root /tmp/otto-p3b && cat /tmp/otto-p3b/otto_output.txt` — the deterministic turn still works (file contains "add a greeting"), regardless of whether bash is registered on this host.

- [ ] **Step 5: Commit**

```bash
git add crates/engine
git commit -m "feat(engine): register sandboxed bash tool when an OS sandbox is available"
```

---

## Task 5: Docs + quality gate

**Files:**
- Modify: `docs/ARCHITECTURE.md`

- [ ] **Step 1: Document the bash tool + sandbox**

In `docs/ARCHITECTURE.md`, extend the `### \`Tool\`` subsection (added in Plan 3a). After its existing paragraph, append:

```markdown
The `bash` tool (`BashTool`) runs shell commands confined by a `SandboxPolicy`: `bwrap` on
Linux / `sandbox-exec` on macOS limit filesystem writes to the workspace root and disable
network. The gate classifies `bash` as `Ask` (shell can't be statically path-vetted), and the
engine registers `bash` ONLY when an OS sandbox backend exists — pairing it with an
`AllowListAskResolver` that permits the now-confined `bash`. With no sandbox backend, `bash`
is absent and `Ask` stays denied (fail-closed). Output is `{stdout, stderr, exit_code}`; a
timeout kills the child via `kill_on_drop`.
```

- [ ] **Step 2: Final gate**

Run: `cargo fmt --all -- --check` (clean), `cargo clippy --workspace --all-targets -- -D warnings` (clean), `cargo test --workspace` — capture the total. (The bash sandboxed test runs or skips depending on whether `bwrap`/`sandbox-exec` is on the host; all non-sandboxed tests pass everywhere.)

- [ ] **Step 3: Commit**

```bash
git add docs/ARCHITECTURE.md
git commit -m "docs: document the sandboxed bash tool in ARCHITECTURE.md"
```

---

## Done — what Plan 3b delivers

otto has a `bash` tool that runs shell commands confined by an OS sandbox (writes limited to the workspace root, network off), surfaced through the same gated `Tool` seam. It is `Ask`-gated and only registered when a sandbox backend is present, so an unconfined shell is never silently available. This is the capability the real Verifier (Plan 4) will use to run `cargo test` / builds.

**Carried forward / deferred (designed-for):**
- The **rmcp MCP-subprocess `Tool` impl** (external + Claude Code MCP servers) — its own plan, naturally paired with the `.claude/` extensions work.
- A **`grep`/search tool** — folds into the retrieval work (Plan 4-adjacent).
- The **interactive `Ask` resolver** (prompt the user) arrives with the UI; until then headless uses `DenyAsk` / the allow-list.
- **Hardening the sandbox profiles** (seccomp filters, tighter macOS profile, configurable bind mounts) and **resolving the two ungated FS paths** (orchestrator `apply_edit` / `ctx.workspace()`) tracked for the Plan 4 coder work.
