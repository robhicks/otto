# otto Plan 4c-2 — Real Verifier Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace `StubVerifier` with a real Verifier that runs the project's build check (`cargo check --offline`) inside the sandboxed `bash` tool and reports pass/fail — activating the Repair loop in production.

**Architecture:** The Verifier detects the project type (via `fs.list`), and for a Cargo project runs `cargo check --offline` through the gated `bash` tool, parsing `exit_code` → pass/fail. It degrades gracefully: no recognized project → "nothing to verify" (ok); `bash` unavailable (no OS sandbox) → "verification skipped" (ok). To make `cargo` usable inside the cleared-env sandbox, the `BashTool`'s curated environment is extended to pass through the (non-secret) Rust toolchain location — `PATH` gains `~/.cargo/bin`, and `CARGO_HOME`/`RUSTUP_HOME` point at the host toolchain. This was verified by probe: a hardened bwrap sandbox with this env runs `cargo metadata --offline` successfully, while the filesystem-read-only / no-network / writes-confined-to-workspace boundary is unchanged.

**Tech Stack:** Rust (edition 2024), serde_json, async-trait, anyhow, tempfile (dev). Runtime: `cargo` + `bwrap` (present on dev host).

---

## Context for the implementer (read once)

Current state (`main`):
- `crates/tools/src/bash.rs` `BashTool::call` builds the spawn command with `.env_clear()` then `.env("PATH", "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin").env("HOME", &self.root).env("TERM", "dumb")`. This cleared minimal env means `cargo` is NOT on PATH and CARGO_HOME/RUSTUP_HOME are unset — so a sandboxed `cargo` cannot find its toolchain. This plan fixes that.
- The sandbox (`sandbox.rs`) is `bwrap --ro-bind / / --bind <root> <root> ... --unshare-net --unshare-pid --unshare-ipc --new-session --die-with-parent` — whole FS read-only except the workspace, no network. The host `~/.cargo` / `~/.rustup` are already readable through `--ro-bind / /`; they are simply not *usable* without the env.
- `crates/agents/src/lib.rs` defines `StubVerifier` (always `ok: true`) and `StubContextFinder` (real ContextFinder is a later plan), plus re-exports `Planner`/`Coder`. The crate has a `verifier`-less module set: `pub mod coder; pub mod parse; pub mod planner;`.
- `crates/engine/src/lib.rs` `build_default_registry` registers `Planner`, `StubContextFinder`, `Coder`, `StubVerifier`. `build_tool_registry` registers `fs.read/write/list` always and `bash` (gated `Ask`, paired with `AllowListAskResolver(["bash"])`) ONLY when `os_sandbox_available()`.
- The orchestrator Repair loop (Plan 4c) re-runs the Coder on `Verify { ok: false }` (bounded, 3 attempts). It's dormant because `StubVerifier` always passes — this plan makes the Verifier able to fail, activating repair.
- Tools are MCP-shaped: agents call `ctx.tools().call(name, json_args) -> Result<Value>`. `fs.list` returns `{"paths": [..]}`. `bash` returns `{"stdout","stderr","exit_code"}`.

**Verified env design (from a probe on this host):** with `--clearenv --setenv PATH "...:{cargo_home}/bin" --setenv HOME <root> --setenv CARGO_HOME {host}/.cargo --setenv RUSTUP_HOME {host}/.rustup`, a sandboxed `cargo --version` and `cargo metadata --offline` both succeed. `cargo check --offline` reads cached deps (read-only) and writes build artifacts to `<root>/target` (writable). If a project's deps aren't cached, `--offline` fails — reported as a verify failure (acceptable v1 behavior; the common case is editing an already-built project).

**Conventions:** stay on branch `feat/plan-4c2-real-verifier`; never detach HEAD; `git add`+`commit` only (no `--amend`); no AI/Claude self-attribution; per-package then workspace gates; `clippy -D warnings` clean; TDD. `impl Agent` uses `ctx: &AgentCtx` (never `<'_>`).

---

## Security note for reviewers (Task 1)

Extending the sandbox env to include the toolchain is the security-relevant change. The key question: does it create new exfiltration or escape? **No.** The host filesystem (including `~/.cargo/credentials.toml` if present) is *already* readable via `--ro-bind / /`; this change only makes `cargo`/`rustc` *usable*, it grants no new read access. Network stays off (`--unshare-net`), so anything readable still cannot be exfiltrated, and writes stay confined to the workspace. The residual — secrets under `~/.cargo` are readable-but-not-exfiltratable — is otto's already-documented v1 posture. Task 1 gets a security-auditor review to confirm this reasoning holds.

---

## File Structure

```
crates/tools/src/bash.rs   # MODIFY: curated_env() passes through the Rust toolchain (PATH/CARGO_HOME/RUSTUP_HOME)
crates/agents/src/
├── verifier.rs            # NEW: real Verifier (detect project → cargo check via bash → pass/fail; graceful degrade)
└── lib.rs                 # MODIFY: remove StubVerifier; add `pub mod verifier; pub use verifier::Verifier;`
crates/engine/src/lib.rs   # MODIFY: build_default_registry registers Verifier (not StubVerifier)
docs/ARCHITECTURE.md       # MODIFY: document the real Verifier + toolchain env
```

---

## Task 1: Toolchain-env passthrough in `BashTool`

**Files:**
- Modify: `crates/tools/src/bash.rs`

- [ ] **Step 1: Add `curated_env()` and apply it**

In `crates/tools/src/bash.rs`, add a free function `curated_env` (above `BashTool` or in an impl — free function is simplest):

```rust
/// The curated environment for a sandboxed command. The host environment is cleared (no
/// credential leakage), then a minimal env is set that also makes the Rust toolchain usable:
/// `PATH` includes the host's `~/.cargo/bin`, and `CARGO_HOME`/`RUSTUP_HOME` point at the host
/// toolchain. These are non-secret locations; the host filesystem is already read-only-readable
/// inside the sandbox, so this grants no new read access — it only makes `cargo`/`rustc` runnable.
/// `HOME` is set separately to the workspace root by the caller.
fn curated_env() -> Vec<(String, String)> {
    let host_home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
    let cargo_home = std::env::var("CARGO_HOME").unwrap_or_else(|_| format!("{host_home}/.cargo"));
    let rustup_home =
        std::env::var("RUSTUP_HOME").unwrap_or_else(|_| format!("{host_home}/.rustup"));
    let path = format!(
        "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:{cargo_home}/bin"
    );
    vec![
        ("PATH".to_string(), path),
        ("TERM".to_string(), "dumb".to_string()),
        ("CARGO_HOME".to_string(), cargo_home),
        ("RUSTUP_HOME".to_string(), rustup_home),
    ]
}
```

In `BashTool::call`, replace the env-setup portion of the command builder. The current code is:
```rust
        let mut cmd = tokio::process::Command::new(program);
        cmd.args(argv)
            .current_dir(&self.root)
            .env_clear()
            .env(
                "PATH",
                "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
            )
            .env("HOME", &self.root)
            .env("TERM", "dumb")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
```
Replace it with (env_clear, then the curated env loop, then HOME = workspace root, then stdio):
```rust
        let mut cmd = tokio::process::Command::new(program);
        cmd.args(argv).current_dir(&self.root).env_clear();
        for (key, val) in curated_env() {
            cmd.env(key, val);
        }
        cmd.env("HOME", &self.root)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
```
(HOME is set AFTER the curated env so it always points at the workspace root, never the host home. `curated_env` does not set HOME.)

- [ ] **Step 2: Update the module doc comment**

The `//!` block at the top of bash.rs mentions "cleared, minimal environment (PATH/HOME/TERM only)". Update that sentence to reflect the toolchain passthrough, e.g.: "...runs with a cleared environment, then a curated minimal env that also makes the Rust toolchain usable (PATH includes `~/.cargo/bin`; CARGO_HOME/RUSTUP_HOME point at the host toolchain) — non-secret locations only, granting no new read access beyond the already-read-only host FS."

- [ ] **Step 3: Add a unit test for `curated_env`**

Add to the bash.rs `#[cfg(test)] mod tests`:
```rust
    #[test]
    fn curated_env_exposes_the_rust_toolchain() {
        let env: std::collections::HashMap<String, String> = curated_env().into_iter().collect();
        let path = env.get("PATH").expect("PATH set");
        assert!(path.contains("/.cargo/bin"), "PATH must include the cargo bin dir: {path}");
        assert!(path.contains("/usr/bin"), "PATH must keep system dirs: {path}");
        assert!(env.get("CARGO_HOME").expect("CARGO_HOME set").ends_with(".cargo"));
        assert!(env.get("RUSTUP_HOME").expect("RUSTUP_HOME set").ends_with(".rustup"));
        assert_eq!(env.get("TERM").map(String::as_str), Some("dumb"));
        // HOME is intentionally NOT in curated_env (the caller sets it to the workspace root).
        assert!(env.get("HOME").is_none());
    }
```

- [ ] **Step 4: Verify the real sandbox still runs (and now runs cargo)**

Run: `cargo test -p otto-tools` (the new `curated_env_exposes_the_rust_toolchain` + existing bash/sandbox tests pass, including `sandboxed_runs_when_backend_available`), `cargo clippy -p otto-tools --all-targets -- -D warnings` (clean), `cargo fmt -p otto-tools` (clean). The existing `runs_echo_and_captures_stdout` etc. still pass (echo is a builtin; the broader PATH doesn't break them).

- [ ] **Step 5: Commit**

```bash
git add crates/tools/src/bash.rs
git commit -m "feat(tools): BashTool passes through the Rust toolchain env (PATH/CARGO_HOME/RUSTUP_HOME)"
```

---

## Task 2: Real `Verifier`

**Files:**
- Create: `crates/agents/src/verifier.rs`
- Modify: `crates/agents/src/lib.rs`

- [ ] **Step 1: Write verifier.rs**

Create `crates/agents/src/verifier.rs`:

```rust
//! The Verifier agent: checks the workspace builds. For a Cargo project it runs
//! `cargo check --offline` inside the sandboxed `bash` tool and reports pass/fail. It degrades
//! gracefully: no recognized project -> "nothing to verify"; `bash` unavailable (no OS sandbox)
//! -> "verification skipped". A failure here drives the orchestrator's Repair loop.

use async_trait::async_trait;
use otto_engine_core::traits::{Agent, AgentCtx};
use otto_engine_core::types::{AgentOutput, AgentRequest};
use serde_json::{json, Value};

pub struct Verifier;

/// Truncate to at most `max` chars on a char boundary (for bounded failure detail).
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push_str("… (truncated)");
        out
    }
}

#[async_trait]
impl Agent for Verifier {
    async fn run(&self, req: AgentRequest, ctx: &AgentCtx) -> anyhow::Result<AgentOutput> {
        let AgentRequest::Verify = req else {
            anyhow::bail!("Verifier received a non-Verify request");
        };

        // Detect the project type by listing the workspace root.
        let files: Vec<String> = match ctx.tools().call("fs.list", json!({})).await {
            Ok(Value::Object(map)) => map
                .get("paths")
                .and_then(Value::as_array)
                .map(|arr| arr.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        let is_cargo = files.iter().any(|f| f == "Cargo.toml");
        if !is_cargo {
            return Ok(AgentOutput::Verify {
                ok: true,
                detail: "no recognized project; nothing to verify".to_string(),
            });
        }

        // Run `cargo check --offline` in the sandbox (stderr merged into stdout via 2>&1).
        let result = ctx
            .tools()
            .call(
                "bash",
                json!({ "command": "cargo check --offline --quiet 2>&1", "timeout_ms": 180000u64 }),
            )
            .await;

        match result {
            Ok(Value::Object(map)) => {
                let exit = map.get("exit_code").and_then(Value::as_i64);
                let stdout = map.get("stdout").and_then(Value::as_str).unwrap_or("");
                if exit == Some(0) {
                    Ok(AgentOutput::Verify {
                        ok: true,
                        detail: "cargo check passed".to_string(),
                    })
                } else {
                    Ok(AgentOutput::Verify {
                        ok: false,
                        detail: truncate(stdout.trim(), 2000),
                    })
                }
            }
            // bash unavailable (no OS sandbox) or denied by the gate -> can't verify safely; skip.
            _ => Ok(AgentOutput::Verify {
                ok: true,
                detail: "verification skipped: bash tool unavailable (no sandbox)".to_string(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use otto_engine_core::tool::{
        AllowListAskResolver, AskResolver, DenyAsk, PermissionGate, Tool, ToolRegistry,
    };
    use otto_engine_core::traits::Workspace;
    use otto_engine_core::types::Edit;
    use otto_providers::LocalProvider;
    use otto_router::SingleProviderRouter;
    use otto_tools::{DefaultPermissionGate, FsListTool};
    use otto_workspace::LocalWorkspace;
    use std::sync::Arc;

    /// A stand-in `bash` tool returning a canned exit code + output, so the Verifier's parse
    /// logic is tested without a real sandbox/cargo.
    struct FakeBash {
        exit_code: i64,
        output: String,
    }
    #[async_trait]
    impl Tool for FakeBash {
        fn name(&self) -> &str {
            "bash"
        }
        async fn call(&self, _args: Value) -> anyhow::Result<Value> {
            Ok(json!({ "stdout": self.output, "stderr": "", "exit_code": self.exit_code }))
        }
    }

    async fn seed_cargo_toml(ws: &LocalWorkspace) {
        ws.apply_edit(&Edit {
            path: std::path::PathBuf::from("Cargo.toml"),
            new_contents: "[package]\nname=\"x\"\n".to_string(),
        })
        .await
        .unwrap();
    }

    fn router() -> SingleProviderRouter {
        SingleProviderRouter::new(Arc::new(LocalProvider::new()))
    }

    /// Build a registry with fs.list over `ws_path`, an optional fake bash, and a resolver that
    /// permits bash when one is registered.
    fn registry(ws_path: &std::path::Path, bash: Option<Arc<dyn Tool>>) -> ToolRegistry {
        let gate: Arc<dyn PermissionGate> = Arc::new(DefaultPermissionGate::new());
        let ask: Arc<dyn AskResolver> = if bash.is_some() {
            Arc::new(AllowListAskResolver::new(vec!["bash".to_string()]))
        } else {
            Arc::new(DenyAsk)
        };
        let mut reg = ToolRegistry::new(gate, ask);
        let ws: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(ws_path));
        reg.register(Arc::new(FsListTool::new(ws)));
        if let Some(b) = bash {
            reg.register(b);
        }
        reg
    }

    async fn run_verifier(ws: &LocalWorkspace, tools: &ToolRegistry) -> AgentOutput {
        let router = router();
        let ctx = AgentCtx::new(&router, ws, tools);
        Verifier.run(AgentRequest::Verify, &ctx).await.unwrap()
    }

    #[tokio::test]
    async fn passes_when_cargo_check_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let ws = LocalWorkspace::new(dir.path());
        seed_cargo_toml(&ws).await;
        let tools = registry(
            dir.path(),
            Some(Arc::new(FakeBash { exit_code: 0, output: "Finished".into() })),
        );
        match run_verifier(&ws, &tools).await {
            AgentOutput::Verify { ok, .. } => assert!(ok),
            other => panic!("expected Verify, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fails_when_cargo_check_errors() {
        let dir = tempfile::tempdir().unwrap();
        let ws = LocalWorkspace::new(dir.path());
        seed_cargo_toml(&ws).await;
        let tools = registry(
            dir.path(),
            Some(Arc::new(FakeBash {
                exit_code: 1,
                output: "error[E0277]: the trait bound is not satisfied".into(),
            })),
        );
        match run_verifier(&ws, &tools).await {
            AgentOutput::Verify { ok, detail } => {
                assert!(!ok);
                assert!(detail.contains("E0277"), "detail should carry the error: {detail}");
            }
            other => panic!("expected Verify, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn skips_when_no_cargo_project() {
        let dir = tempfile::tempdir().unwrap();
        let ws = LocalWorkspace::new(dir.path());
        // No Cargo.toml seeded. A fake bash is present but must NOT be called.
        let tools = registry(
            dir.path(),
            Some(Arc::new(FakeBash { exit_code: 99, output: "should not run".into() })),
        );
        match run_verifier(&ws, &tools).await {
            AgentOutput::Verify { ok, detail } => {
                assert!(ok);
                assert!(detail.contains("nothing to verify"));
            }
            other => panic!("expected Verify, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn skips_when_bash_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let ws = LocalWorkspace::new(dir.path());
        seed_cargo_toml(&ws).await;
        // Cargo project, but NO bash tool registered (and DenyAsk) -> bash call fails -> skip.
        let tools = registry(dir.path(), None);
        match run_verifier(&ws, &tools).await {
            AgentOutput::Verify { ok, detail } => {
                assert!(ok);
                assert!(detail.contains("skipped"));
            }
            other => panic!("expected Verify, got {other:?}"),
        }
    }
}
```

- [ ] **Step 2: Update lib.rs — declare verifier module, re-export, remove StubVerifier**

In `crates/agents/src/lib.rs`:
1. Add `pub mod verifier;` to the module declarations.
2. Add `pub use verifier::Verifier;`.
3. DELETE the `StubVerifier` struct + its `impl Agent` block.
4. Update the crate-level `//!` doc comment line that says "`StubContextFinder` and `StubVerifier` remain stubs" → "`StubContextFinder` remains a stub until its real version lands." (`Verifier` is now real.)
5. Fix imports: after removing `StubVerifier`, the only agent left defined in lib.rs is `StubContextFinder`, which uses `Agent`, `AgentCtx`, `AgentOutput`, `AgentRequest`, `serde_json::Value`, `async_trait`. Run clippy `-D warnings` and remove any now-unused import.

- [ ] **Step 3: Test**

Run: `cargo test -p otto-agents` (the 4 verifier tests + planner/coder/parse/context_finder pass), `cargo clippy -p otto-agents --all-targets -- -D warnings` (clean), `cargo fmt -p otto-agents` (clean). (Do NOT run `--workspace` yet — `otto-engine` still references `StubVerifier` until Task 3.)

- [ ] **Step 4: Commit**

```bash
git add crates/agents
git commit -m "feat(agents): real Verifier — cargo check via sandboxed bash, graceful degrade"
```

---

## Task 3: Wire the real Verifier into the engine

**Files:**
- Modify: `crates/engine/src/lib.rs`

- [ ] **Step 1: Register `Verifier`**

In `crates/engine/src/lib.rs`, change the agents import and `build_default_registry`. The import currently reads `use otto_agents::{Coder, Planner, StubContextFinder, StubVerifier};`. Change to:
```rust
use otto_agents::{Coder, Planner, StubContextFinder, Verifier};
```
In `build_default_registry`, change the Verifier registration:
```rust
    registry.register(Role::Verifier, Arc::new(Verifier));
```
(Update the doc comment on `build_default_registry` to note the Verifier is now real: "real LLM-backed Planner + Coder + a real Verifier (cargo check via sandboxed bash); ContextFinder remains a stub.")

- [ ] **Step 2: Full workspace test + CLI smoke**

Run: `cargo test --workspace`. ALL pass. In particular the engine integration test `full_turn_writes_parsed_edit_and_completes_ok` still passes: its tempdir workspace gets the Coder's `otto_output.txt` (no `Cargo.toml`), so the real Verifier lists files, finds no Cargo project, returns `ok: true` ("nothing to verify"), and the turn completes ok with the edit written.

Run: `cargo clippy --workspace --all-targets -- -D warnings` (clean), `cargo fmt --all -- --check` (clean).

CLI smoke (offline, no LLM — honest no-op turn, real Verifier finds nothing to verify in the empty dir): `mkdir -p /tmp/otto-p4c2 && cargo run -p otto-engine -- run "add a greeting" --root /tmp/otto-p4c2` → prints the event stream ending `turn ok = true` (Coder falls back to no edits offline; Verifier sees no Cargo.toml → ok), writes nothing. Confirm it completes without error.

- [ ] **Step 3: Commit**

```bash
git add crates/engine/src/lib.rs
git commit -m "feat(engine): register the real Verifier; Repair loop is now live"
```

---

## Task 4: Docs + quality gate

**Files:**
- Modify: `docs/ARCHITECTURE.md`

- [ ] **Step 1: Document the real Verifier**

In `docs/ARCHITECTURE.md`, in the `### \`Agent\`` subsection (where the real Planner/Coder are described), append:

```markdown
The `Verifier` is real: it detects the project (via `fs.list`) and, for a Cargo project, runs
`cargo check --offline` inside the sandboxed `bash` tool, reporting pass/fail (a non-zero exit
becomes `Verify { ok: false }` with the truncated build output as detail, which drives the
orchestrator's Repair loop). It degrades safely — no recognized project → "nothing to verify";
`bash` unavailable (no OS sandbox) → "verification skipped". To run `cargo` inside the
cleared-env sandbox, `BashTool` passes through the non-secret Rust toolchain location (`PATH`
includes `~/.cargo/bin`; `CARGO_HOME`/`RUSTUP_HOME` point at the host toolchain); this grants
no new read access (the host FS is already read-only-readable in the sandbox) and network stays
off, so the read-but-no-exfil posture is unchanged. `ContextFinder` remains a stub.
```

- [ ] **Step 2: Final gate**

Run: `cargo fmt --all -- --check` (clean), `cargo clippy --workspace --all-targets -- -D warnings` (clean), `cargo test --workspace` — capture the per-crate breakdown + summed total.

- [ ] **Step 3: Commit**

```bash
git add docs/ARCHITECTURE.md
git commit -m "docs: document the real Verifier and toolchain-env passthrough"
```

---

## Done — what Plan 4c-2 delivers

otto now actually verifies its work: for a Cargo project, the Verifier runs `cargo check --offline` in the sandbox and fails the turn (driving repair) when the build is broken. The `BashTool` makes the toolchain usable inside the cleared-env sandbox without weakening the FS/network boundary. With this, the full agentic loop is live end-to-end against a real model: **plan → generate gated edits → cargo-check → repair on failure (escalating local→remote) → done**.

**Carried forward / deferred:**
- Other project types (npm `tsc`/`npm test`, Python, etc.) — the Verifier currently recognizes only Cargo; generalize via a detected/configured verify command later.
- `--offline` limitation: a Coder that adds an uncached dependency will fail verification (no network to fetch it); a future plan may allow a vetted dependency-fetch step.
- The remaining tracked items: real `ContextFinder` + retrieval (Plan 4d); Coder-fallback observability; threading milestones into the Coder; a read-only workspace view for untrusted agents.
