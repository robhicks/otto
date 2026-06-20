# Sub-project D — Diff Approval Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let a human approve or reject each Coder edit before it is applied — when approval mode is on, an edit the gate marks `Ask` pauses the turn, the UI renders the proposed diff with Approve/Reject, and the edit is applied only on explicit approval.

**Architecture:** Approval is a new behavior on the orchestrator's existing per-edit `Ask` branch (`Allow`/`Deny` untouched, sensitive floor still `Deny`s first). A new async `Approver` seam in `engine-core` carries the verdict; `DenyApprover` keeps CLI/headless fail-closed. `serve` reads the socket *concurrently* with the running turn (`socket.split()` + `tokio::select!`), routing `ApproveDiff` frames to a per-connection `ApprovalRegistry`. An opt-in `serve --approve-edits` flag swaps in a gate that upgrades non-sensitive `fs.write` from `Allow` to `Ask`. The UI renders the diff from old+new contents via a pure host-tested function.

**Tech Stack:** Rust (edition 2024), tokio, axum + axum-server, `futures-util` (split/select), `uuid`; Leptos 0.8 CSR (WASM) for the UI; tests with `tokio`, `tempfile`, `tokio-tungstenite`.

**Spec:** `docs/superpowers/specs/2026-06-19-ui-diff-approval-design.md`

**Conventions to respect (from CLAUDE.md):**
- The orchestrator applies edits **only** on an explicit approval — reject/deny/disconnect all fail closed.
- The sensitive-path floor (`Deny`) is evaluated before approval is ever reached.
- Default `serve`, CLI `run`, and the offline determinism suite must behave exactly as today (approval is opt-in; the offline path never produces an `Ask` write).
- `protocol` depends only on serde(+uuid); `ui` depends only on `protocol` (+ its existing UI deps).
- Run `cargo fmt --all` before each commit. **No Claude self-attribution in commit messages** (no `Co-Authored-By: Claude`, no "Generated with" footer, no emoji marker).
- The `ui/` crate is **not** a workspace member — build/test it from inside `ui/` (`cd ui && cargo test`, `cd ui && cargo build --target wasm32-unknown-unknown`). Workspace tasks (1–8) do not touch it.

---

## Task 1: Protocol — `ApproveDiff` command + `ApprovalRequest` event

**Files:**
- Modify: `crates/protocol/src/lib.rs` (the `Command` enum ~line 37, the `EventKind` enum ~line 45, tests ~line 117)

- [ ] **Step 1: Write the failing round-trip tests**

Add these two tests inside `mod tests` in `crates/protocol/src/lib.rs` (after `command_round_trips_through_json`):

```rust
    #[test]
    fn approve_diff_command_round_trips() {
        let cmd = Command::ApproveDiff {
            session: SessionId::new(),
            id: Uuid::from_u128(7),
            approved: true,
        };
        let back: Command = serde_json::from_str(&serde_json::to_string(&cmd).unwrap()).unwrap();
        assert_eq!(cmd, back);
    }

    #[test]
    fn approval_request_event_round_trips() {
        let event = Event {
            seq: 4,
            session: SessionId::new(),
            kind: EventKind::ApprovalRequest {
                id: Uuid::from_u128(9),
                path: PathBuf::from("src/a.rs"),
                old: Some("old line\n".to_string()),
                new: "new line\n".to_string(),
            },
        };
        let back: Event = serde_json::from_str(&serde_json::to_string(&event).unwrap()).unwrap();
        assert_eq!(event, back);
        // A new file carries `old: None`.
        let new_file = EventKind::ApprovalRequest {
            id: Uuid::from_u128(1),
            path: PathBuf::from("new.rs"),
            old: None,
            new: "x".to_string(),
        };
        let back: EventKind =
            serde_json::from_str(&serde_json::to_string(&new_file).unwrap()).unwrap();
        assert_eq!(new_file, back);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p otto-protocol approve`
Expected: FAIL — `no variant named ApproveDiff` / `no variant named ApprovalRequest`.

- [ ] **Step 3: Add the variants**

In the `Command` enum (after `Abort { session: SessionId },`):

```rust
    ApproveDiff {
        session: SessionId,
        id: Uuid,
        approved: bool,
    },
```

In the `EventKind` enum (after `FileEdit { path: PathBuf, bytes_written: u64 },`):

```rust
    /// The Coder proposes an edit that needs human approval. `old` is the file's current
    /// contents (`None` if it does not exist yet); `new` is the proposed contents. The UI
    /// renders the diff and replies with `Command::ApproveDiff { id, approved }`.
    ApprovalRequest {
        id: Uuid,
        path: PathBuf,
        old: Option<String>,
        new: String,
    },
```

(`use uuid::Uuid;` and `use std::path::PathBuf;` are already imported at the top of the file.)

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p otto-protocol`
Expected: PASS (all protocol tests, including the new two).

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
git add crates/protocol/src/lib.rs
git commit -m "feat(protocol): ApproveDiff command + ApprovalRequest event for diff approval"
```

---

## Task 2: engine-core — `Approver` seam + `DenyApprover`

**Files:**
- Modify: `crates/engine-core/Cargo.toml` (add `uuid`)
- Modify: `crates/engine-core/src/tool.rs` (add trait + impl + test)
- Modify: `crates/engine-core/src/lib.rs` (re-export)

- [ ] **Step 1: Add the `uuid` dependency**

In `crates/engine-core/Cargo.toml`, under `[dependencies]`, add:

```toml
uuid = { workspace = true }
```

- [ ] **Step 2: Write the failing test**

At the top of `crates/engine-core/src/tool.rs`, ensure the imports include `std::path::Path` and `uuid::Uuid` (add to the existing `use` block):

```rust
use std::path::Path;
use uuid::Uuid;
```

Add this test inside `mod tests` in `tool.rs`:

```rust
    #[tokio::test]
    async fn deny_approver_always_rejects() {
        let a = DenyApprover;
        assert!(!a.request(Uuid::from_u128(0), Path::new("x.rs"), None, "new").await);
        assert!(
            !a.request(Uuid::from_u128(1), Path::new("y.rs"), Some("old"), "new")
                .await
        );
    }
```

- [ ] **Step 3: Run the test to verify it fails**

Run: `cargo test -p otto-engine-core deny_approver`
Expected: FAIL — `cannot find type DenyApprover`.

- [ ] **Step 4: Add the `Approver` trait + `DenyApprover`**

In `crates/engine-core/src/tool.rs`, after the `AskResolver` trait / `DenyAsk` block (before `AllowListAskResolver`):

```rust
/// Resolves an interactive approval for a proposed edit (the `Ask`-on-`fs.write` path).
/// Async because the verdict is a round-trip to a human/UI. Implementations MUST fail closed
/// (return `false`) when they cannot obtain an answer (e.g. a closed channel on disconnect).
#[async_trait]
pub trait Approver: Send + Sync {
    /// `true` = apply the edit, `false` = skip it. `old` is the file's current contents
    /// (`None` if the file does not exist yet); `new` is the proposed contents.
    async fn request(&self, id: Uuid, path: &Path, old: Option<&str>, new: &str) -> bool;
}

/// Headless default: never approve (≙ the orchestrator's prior `Ask → skip` behavior).
pub struct DenyApprover;

#[async_trait]
impl Approver for DenyApprover {
    async fn request(&self, _id: Uuid, _path: &Path, _old: Option<&str>, _new: &str) -> bool {
        false
    }
}
```

- [ ] **Step 5: Re-export from the crate root**

In `crates/engine-core/src/lib.rs`, extend the `pub use tool::{ ... }` line to include `Approver, DenyApprover`:

```rust
pub use tool::{
    AllowListAskResolver, Approver, AskResolver, Decision, DenyApprover, DenyAsk, PermissionGate,
    Tool, ToolRegistry,
};
```

- [ ] **Step 6: Run the test to verify it passes**

Run: `cargo test -p otto-engine-core`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
cargo fmt --all
git add crates/engine-core/Cargo.toml crates/engine-core/src/tool.rs crates/engine-core/src/lib.rs
git commit -m "feat(engine-core): async Approver seam + DenyApprover default"
```

---

## Task 3: Orchestrator — route `Ask` edits through the approver

**Files:**
- Modify: `crates/engine-core/src/orchestrator.rs` (struct + edit loop + all test literals + new tests)
- Modify: `crates/engine/src/service.rs` (the `Orchestrator { … }` literal at ~line 118 — the struct gained fields)

This task changes the `Orchestrator` struct, so every `Orchestrator { … }` literal must gain the two new fields or the workspace won't compile. There are 6 in `orchestrator.rs` tests and 1 in `service.rs`.

- [ ] **Step 1: Add the two struct fields**

In `crates/engine-core/src/orchestrator.rs`, extend the struct:

```rust
pub struct Orchestrator<'a> {
    pub registry: &'a AgentRegistry,
    pub router: &'a dyn Router,
    pub workspace: &'a dyn Workspace,
    pub tools: &'a ToolRegistry,
    /// Resolves an `Ask` verdict on a proposed edit to apply/skip (interactive approval).
    pub approver: &'a dyn Approver,
    /// Mints the correlation id for an `ApprovalRequest`. Injected by the engine layer so the
    /// orchestrator stays free of nondeterministic calls (the offline path never reaches it).
    pub next_id: &'a (dyn Fn() -> Uuid + Send + Sync),
}
```

Add imports at the top of the file:

```rust
use uuid::Uuid;
use crate::tool::Approver;
```

(and ensure `Decision` and `EventKind` are already imported — they are.)

- [ ] **Step 2: Replace the edit-apply loop's gate handling**

In `run_turn`, find the `for edit in &edits { … }` block (currently the `if self.tools.check(...) != Decision::Allow { … continue }` form) and replace the whole `for` body with:

```rust
            for edit in &edits {
                let check_args = serde_json::json!({ "path": edit.path.to_string_lossy() });
                match self.tools.check("fs.write", &check_args) {
                    Decision::Allow => {}
                    Decision::Deny => {
                        emit.emit(EventKind::Log {
                            message: format!(
                                "edit to {} denied by permission gate; skipped",
                                edit.path.display()
                            ),
                        });
                        continue;
                    }
                    Decision::Ask => {
                        // Read current contents for the diff (None if the file does not exist).
                        let old = self
                            .workspace
                            .read(&edit.path)
                            .await
                            .ok()
                            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned());
                        let id = (self.next_id)();
                        emit.emit(EventKind::ApprovalRequest {
                            id,
                            path: edit.path.clone(),
                            old: old.clone(),
                            new: edit.new_contents.clone(),
                        });
                        let approved = self
                            .approver
                            .request(id, &edit.path, old.as_deref(), &edit.new_contents)
                            .await;
                        if !approved {
                            emit.emit(EventKind::Log {
                                message: format!(
                                    "edit to {} rejected; skipped",
                                    edit.path.display()
                                ),
                            });
                            continue;
                        }
                    }
                }
                let bytes_written = self.workspace.apply_edit(edit).await?;
                emit.emit(EventKind::FileEdit {
                    path: edit.path.clone(),
                    bytes_written,
                });
            }
```

- [ ] **Step 3: Add test helpers + fix existing test literals**

At the top of `mod tests` in `orchestrator.rs`, add (near the other helpers, after the imports):

```rust
    use crate::tool::{Approver, DenyApprover};
    use uuid::Uuid;

    /// A fixed id so `ApprovalRequest` assertions are deterministic in tests.
    fn test_id() -> Uuid {
        Uuid::from_u128(0)
    }
```

The existing `mod tests` already imports `Decision`, `DenyAsk`, etc. Now update **each** of the 6 `let orch = Orchestrator { … };` literals to add the two fields. For every one that does **not** specifically test approval (all the existing ones), add:

```rust
            approver: &DenyApprover,
            next_id: &test_id,
```

So e.g. the first becomes:

```rust
        let orch = Orchestrator {
            registry: &reg,
            router: &router,
            workspace: &workspace,
            tools: &tools,
            approver: &DenyApprover,
            next_id: &test_id,
        };
```

Apply the identical two-line addition to all 6 literals (the tests: `run_turn_drives_full_spine_and_emits_ordered_events`, `run_turn_errors_when_a_role_is_missing`, `denied_edit_is_skipped_and_logged`, `ask_verdict_also_skips_edit_fail_closed`, `flaky_verifier_triggers_repair_then_succeeds`, `repair_exhaustion_fails_the_turn`).

Note: `ask_verdict_also_skips_edit_fail_closed` uses `TestAskWriteGate` → now the `Ask` branch calls `approver.request(...)`; with `DenyApprover` it returns `false` → still skips. The assertion `edits == 0` still holds.

- [ ] **Step 4: Add the new approver tests**

Add to `mod tests` in `orchestrator.rs`:

```rust
    /// Records each approval request and returns a fixed verdict.
    struct ScriptedApprover {
        approve: bool,
        seen: Mutex<Vec<(Uuid, PathBuf, Option<String>, String)>>,
    }
    #[async_trait]
    impl Approver for ScriptedApprover {
        async fn request(&self, id: Uuid, path: &Path, old: Option<&str>, new: &str) -> bool {
            self.seen.lock().unwrap().push((
                id,
                path.to_path_buf(),
                old.map(|s| s.to_string()),
                new.to_string(),
            ));
            self.approve
        }
    }

    #[tokio::test]
    async fn ask_edit_approved_is_applied_and_emits_request() {
        let reg = registry(); // OneEditCoder → edit to out.txt
        let router = FakeRouter;
        let workspace = RecordingWorkspace::default();
        let tools = ToolRegistry::new(Arc::new(TestAskWriteGate), Arc::new(DenyAsk));
        let approver = ScriptedApprover {
            approve: true,
            seen: Mutex::new(Vec::new()),
        };
        let orch = Orchestrator {
            registry: &reg,
            router: &router,
            workspace: &workspace,
            tools: &tools,
            approver: &approver,
            next_id: &test_id,
        };

        let events: Arc<Mutex<Vec<EventKind>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = {
            let events = Arc::clone(&events);
            move |kind: EventKind| events.lock().unwrap().push(kind)
        };

        let outcome = orch.run_turn(SessionId::new(), "x", &sink).await.unwrap();
        assert_eq!(outcome, TurnOutcome { ok: true });
        // Approved → edit applied.
        assert_eq!(workspace.edits.lock().unwrap().len(), 1);

        // The approver saw the proposed edit (RecordingWorkspace::read yields empty → old = "").
        let seen = approver.seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].0, Uuid::from_u128(0));
        assert_eq!(seen[0].1, PathBuf::from("out.txt"));
        assert_eq!(seen[0].3, "hi");

        // An ApprovalRequest event was emitted with the same id/path/new.
        let recorded = events.lock().unwrap().clone();
        assert!(recorded.iter().any(|e| matches!(
            e,
            EventKind::ApprovalRequest { id, path, new, .. }
            if *id == Uuid::from_u128(0) && path == &PathBuf::from("out.txt") && new == "hi"
        )));
    }

    #[tokio::test]
    async fn ask_edit_rejected_is_skipped_but_turn_completes() {
        let reg = registry();
        let router = FakeRouter;
        let workspace = RecordingWorkspace::default();
        let tools = ToolRegistry::new(Arc::new(TestAskWriteGate), Arc::new(DenyAsk));
        let approver = ScriptedApprover {
            approve: false,
            seen: Mutex::new(Vec::new()),
        };
        let orch = Orchestrator {
            registry: &reg,
            router: &router,
            workspace: &workspace,
            tools: &tools,
            approver: &approver,
            next_id: &test_id,
        };

        let events: Arc<Mutex<Vec<EventKind>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = {
            let events = Arc::clone(&events);
            move |kind: EventKind| events.lock().unwrap().push(kind)
        };

        let outcome = orch.run_turn(SessionId::new(), "x", &sink).await.unwrap();
        assert_eq!(outcome, TurnOutcome { ok: true });
        // Rejected → no edit applied.
        assert_eq!(workspace.edits.lock().unwrap().len(), 0);
        let recorded = events.lock().unwrap().clone();
        assert!(recorded.iter().any(|e| matches!(
            e,
            EventKind::Log { message } if message.contains("rejected")
        )));
        assert!(
            !recorded
                .iter()
                .any(|e| matches!(e, EventKind::FileEdit { .. }))
        );
    }
```

(`PathBuf`, `Path`, `Mutex`, `Arc`, `async_trait` are already imported in this `mod tests`.)

- [ ] **Step 5: Fix the `service.rs` Orchestrator literal**

In `crates/engine/src/service.rs`, the `run_prompt` body builds an `Orchestrator` (~line 118). The struct now needs `approver` + `next_id`. For this task (run_prompt does not yet take an approver), supply the headless default. Add a `use` at the top of `service.rs`:

```rust
use otto_engine_core::tool::{Decision, DenyApprover, ToolRegistry};
```

(extend the existing `tool::{Decision, ToolRegistry}` import to include `DenyApprover`.)

Inside the spawned task in `run_prompt`, just before `let orchestrator = Orchestrator { … };`, add a local minter, and add the two fields:

```rust
                let next_id = || uuid::Uuid::new_v4();
                let orchestrator = Orchestrator {
                    registry: &registry,
                    router: &*router,
                    workspace: &*workspace,
                    tools: &tools,
                    approver: &DenyApprover,
                    next_id: &next_id,
                };
```

- [ ] **Step 6: Run the tests**

Run: `cargo test -p otto-engine-core && cargo test -p otto-engine service::`
Expected: PASS — including the two new orchestrator tests; existing orchestrator + service tests unchanged.

- [ ] **Step 7: Commit**

```bash
cargo fmt --all
git add crates/engine-core/src/orchestrator.rs crates/engine/src/service.rs
git commit -m "feat(engine-core): route Ask edits through the Approver (emit ApprovalRequest, apply on approval)"
```

---

## Task 4: engine — `ApprovalModeGate` + `build_tool_registry_approving`

**Files:**
- Create: `crates/engine/src/approval.rs`
- Modify: `crates/engine/src/lib.rs` (module decl, re-export, refactor `build_tool_registry`)

- [ ] **Step 1: Write the failing test (the gate)**

Create `crates/engine/src/approval.rs`:

```rust
//! Approval-mode gate: an opt-in wrapper that turns ordinary `fs.write` edits into `Ask`
//! verdicts so they require interactive approval. The interactive resolution itself lives in
//! the serve layer (`InteractiveApprover`); this is only the policy decorator.

use std::sync::Arc;

use otto_engine_core::tool::{Decision, PermissionGate};
use serde_json::Value;

/// Wraps an inner gate, upgrading a *permitted* `fs.write` from `Allow` to `Ask`. A sensitive
/// `Deny` and every other classification (incl. `bash → Ask`) pass through unchanged, so the
/// inviolable sensitive-path floor is preserved.
pub struct ApprovalModeGate {
    inner: Arc<dyn PermissionGate>,
}

impl ApprovalModeGate {
    pub fn new(inner: Arc<dyn PermissionGate>) -> Self {
        Self { inner }
    }
}

impl PermissionGate for ApprovalModeGate {
    fn evaluate(&self, tool: &str, args: &Value) -> Decision {
        let inner = self.inner.evaluate(tool, args);
        if tool == "fs.write" && inner == Decision::Allow {
            Decision::Ask
        } else {
            inner
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use otto_tools::DefaultPermissionGate;
    use serde_json::json;

    fn gate() -> ApprovalModeGate {
        ApprovalModeGate::new(Arc::new(DefaultPermissionGate::new()))
    }

    #[test]
    fn upgrades_ordinary_write_allow_to_ask() {
        assert_eq!(
            gate().evaluate("fs.write", &json!({"path": "src/a.rs"})),
            Decision::Ask
        );
    }

    #[test]
    fn sensitive_write_still_denied() {
        assert_eq!(
            gate().evaluate("fs.write", &json!({"path": ".env"})),
            Decision::Deny
        );
    }

    #[test]
    fn reads_and_bash_pass_through() {
        assert_eq!(
            gate().evaluate("fs.read", &json!({"path": "src/a.rs"})),
            Decision::Allow
        );
        assert_eq!(
            gate().evaluate("bash", &json!({"command": "ls"})),
            Decision::Ask
        );
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p otto-engine approval::`
Expected: FAIL — `file not found for module approval` / unresolved `ApprovalModeGate` (module not declared yet).

- [ ] **Step 3: Declare the module, re-export, and refactor `build_tool_registry`**

In `crates/engine/src/lib.rs`:

1. Add the module declaration near the other `mod` lines:

```rust
mod approval;
```

2. Add to the re-exports (near `pub use service::{…}`):

```rust
pub use approval::ApprovalModeGate;
```

3. Add `PermissionGate` to the engine-core tool import at the top of `lib.rs` (the line currently importing `AllowListAskResolver, AskResolver, DenyAsk, ToolRegistry`):

```rust
use otto_engine_core::tool::{
    AllowListAskResolver, AskResolver, DenyAsk, PermissionGate, ToolRegistry,
};
```

4. Replace the existing `pub fn build_tool_registry(...) -> ToolRegistry { … }` with a thin pair over a private builder:

```rust
/// Build the tool registry with the default gate (ordinary writes auto-`Allow`ed).
pub fn build_tool_registry(workspace: Arc<dyn Workspace>, root: PathBuf) -> ToolRegistry {
    build_tool_registry_inner(workspace, root, false)
}

/// Build the tool registry in **approval mode**: ordinary `fs.write` is gated `Ask` (the
/// interactive approver applies it only on an explicit approval). The sensitive floor is
/// unchanged. Used by `serve --approve-edits`.
pub fn build_tool_registry_approving(workspace: Arc<dyn Workspace>, root: PathBuf) -> ToolRegistry {
    build_tool_registry_inner(workspace, root, true)
}

fn build_tool_registry_inner(
    workspace: Arc<dyn Workspace>,
    root: PathBuf,
    approve_edits: bool,
) -> ToolRegistry {
    let sandboxed = os_sandbox_available();
    let ask: Arc<dyn AskResolver> = if sandboxed {
        Arc::new(AllowListAskResolver::new(vec!["bash".to_string()]))
    } else {
        Arc::new(DenyAsk)
    };

    let base_gate: Arc<dyn PermissionGate> = Arc::new(DefaultPermissionGate::new());
    let gate: Arc<dyn PermissionGate> = if approve_edits {
        Arc::new(ApprovalModeGate::new(base_gate))
    } else {
        base_gate
    };

    let mut registry = ToolRegistry::new(gate, ask);
    // fs.read / fs.list need only the read-only view; fs.write holds the full workspace.
    let read_workspace: Arc<dyn WorkspaceRead> = workspace.clone();
    registry.register(Arc::new(FsReadTool::new(read_workspace.clone())));
    registry.register(Arc::new(FsWriteTool::new(Arc::clone(&workspace))));
    registry.register(Arc::new(FsListTool::new(read_workspace)));

    if sandboxed {
        registry.register(Arc::new(BashTool::new(
            root,
            SandboxPolicy::Os { allow_net: false },
        )));
    }

    registry
}
```

(Keep the existing doc comment above `build_tool_registry` describing the sandbox/bash rule — move it above `build_tool_registry_inner` or leave a short pointer. The `AllowListAskResolver` still only ever auto-allows `bash`, never `fs.write`, so an `Ask` write is resolved by the `Approver`, not the ask-resolver — note this in a one-line comment.)

Add a one-line note before `let ask` in `build_tool_registry_inner`:

```rust
    // NB: the ask-resolver only ever auto-allows `bash`. An `Ask` on `fs.write` (approval mode)
    // is resolved by the orchestrator's `Approver`, never here — so writes can't slip through.
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p otto-engine approval:: && cargo build -p otto-engine`
Expected: PASS + clean build (existing `build_tool_registry` callers unchanged — signature preserved).

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
git add crates/engine/src/approval.rs crates/engine/src/lib.rs
git commit -m "feat(engine): ApprovalModeGate + build_tool_registry_approving (opt-in Ask-on-write)"
```

---

## Task 5: `EngineService::run_prompt_with_approver`

**Files:**
- Modify: `crates/engine/src/service.rs` (split `run_prompt` into a public default + an approver-taking variant)

- [ ] **Step 1: Add the approver-taking method, delegate the existing one**

In `crates/engine/src/service.rs`, add `Approver` to the tool import:

```rust
use otto_engine_core::tool::{Approver, Decision, DenyApprover, ToolRegistry};
```

Replace the existing `pub async fn run_prompt(&self, session, goal, sink) -> … { … }` so that `run_prompt` delegates and the real body moves into `run_prompt_with_approver`:

```rust
    /// Run one turn with the headless default approver (`DenyApprover`): an `Ask` edit is
    /// skipped (fail-closed). (≙ `Command::SendPrompt` from a non-interactive caller.)
    pub async fn run_prompt(
        &self,
        session: SessionId,
        goal: &str,
        sink: &mut dyn EventSink,
    ) -> anyhow::Result<TurnOutcome> {
        self.run_prompt_with_approver(session, goal, sink, Arc::new(DenyApprover))
            .await
    }

    /// As `run_prompt`, but resolves `Ask` edits through `approver` (the serve layer supplies an
    /// interactive one). The approver is moved into the spawned turn task.
    pub async fn run_prompt_with_approver(
        &self,
        session: SessionId,
        goal: &str,
        sink: &mut dyn EventSink,
        approver: Arc<dyn Approver>,
    ) -> anyhow::Result<TurnOutcome> {
        // … existing run_prompt body …
    }
```

Move the **entire** current body of `run_prompt` into `run_prompt_with_approver`. Then, inside the spawned task, capture the approver and use a real id minter (replace the Task-3 placeholder fields):

```rust
        let handle = {
            let registry = Arc::clone(&self.registry);
            let router = Arc::clone(&self.router);
            let workspace = Arc::clone(&self.workspace);
            let tools = Arc::clone(&self.tools);
            let goal = goal.to_string();
            let counter = Arc::new(AtomicU64::new(start_seq));
            let approver = Arc::clone(&approver);
            tokio::spawn(async move {
                let sink_fn = move |kind: EventKind| {
                    let seq = counter.fetch_add(1, Ordering::SeqCst);
                    let _ = tx.send(Event { seq, session, kind });
                };
                let next_id = || uuid::Uuid::new_v4();
                let orchestrator = Orchestrator {
                    registry: &registry,
                    router: &*router,
                    workspace: &*workspace,
                    tools: &tools,
                    approver: &*approver,
                    next_id: &next_id,
                };
                orchestrator.run_turn(session, &goal, &sink_fn).await
            })
        };
```

(Remove the now-unused `DenyApprover` reference inside the task that Task 3 added; `DenyApprover` is still imported because the public `run_prompt` uses it.)

- [ ] **Step 2: Run the existing service tests to verify no behavior change**

Run: `cargo test -p otto-engine service::`
Expected: PASS — the existing tests call `run_prompt` (now delegating to `DenyApprover`), behavior identical.

- [ ] **Step 3: Commit**

```bash
cargo fmt --all
git add crates/engine/src/service.rs
git commit -m "feat(engine): run_prompt_with_approver; run_prompt delegates with DenyApprover"
```

---

## Task 6: serve — concurrent socket + interactive approver

**Files:**
- Modify: `crates/engine/Cargo.toml` (promote `futures-util` to a normal dependency)
- Modify: `crates/engine/src/serve.rs` (split socket, `WsSink`/`send_msg` over the writer half, `ApprovalRegistry` + `InteractiveApprover`, concurrent `handle_socket`)

This is the structural change. Verified by the **existing** serve tests staying green (no behavior change while approval is off); the approval-specific tests come in Task 7.

- [ ] **Step 1: Promote `futures-util` to a dependency**

In `crates/engine/Cargo.toml`, under `[dependencies]` add:

```toml
futures-util = "0.3"
```

(It is already listed under `[dev-dependencies]`; a normal dependency also covers tests.)

- [ ] **Step 2: Add imports + the approval registry/approver to `serve.rs`**

At the top of `crates/engine/src/serve.rs`, add:

```rust
use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use futures_util::stream::{SplitSink, StreamExt};
use futures_util::SinkExt;
use otto_engine_core::tool::Approver;
use tokio::sync::oneshot;
use uuid::Uuid;
```

(Keep the existing imports; `Command`/`Event`/`ServerMessage`/`WorkspaceRequest` etc. stay. `axum::extract::ws::{Message, WebSocket, WebSocketUpgrade}` stays.)

Add, near the top of the file (after the `ConnectParams`/`ServeState` definitions):

```rust
/// The writer half of a split WebSocket — what events and frames are sent through.
type WsWriter = SplitSink<WebSocket, Message>;

/// Per-connection registry of pending edit approvals, keyed by the `ApprovalRequest` id.
/// Shared between the running turn's `InteractiveApprover` and the socket-reader that routes
/// inbound `ApproveDiff` frames. Dropping a sender (on `clear`/disconnect) resolves the awaiting
/// `request()` to `false` — the single fail-closed rule.
#[derive(Clone, Default)]
struct ApprovalRegistry {
    pending: Arc<Mutex<HashMap<Uuid, oneshot::Sender<bool>>>>,
}

impl ApprovalRegistry {
    fn new() -> Self {
        Self::default()
    }
    fn insert(&self, id: Uuid, tx: oneshot::Sender<bool>) {
        self.pending.lock().unwrap().insert(id, tx);
    }
    fn resolve(&self, id: Uuid, approved: bool) {
        if let Some(tx) = self.pending.lock().unwrap().remove(&id) {
            let _ = tx.send(approved);
        }
    }
    /// Drop all pending senders → every awaiting `request()` resolves `false` (fail-closed).
    fn clear(&self) {
        self.pending.lock().unwrap().clear();
    }
}

/// Approver that surfaces each request to the connected UI and awaits its `ApproveDiff` reply.
struct InteractiveApprover {
    registry: ApprovalRegistry,
}

impl InteractiveApprover {
    fn new(registry: ApprovalRegistry) -> Self {
        Self { registry }
    }
}

#[async_trait::async_trait]
impl Approver for InteractiveApprover {
    async fn request(&self, id: Uuid, _path: &Path, _old: Option<&str>, _new: &str) -> bool {
        let (tx, rx) = oneshot::channel();
        self.registry.insert(id, tx);
        // A closed channel (disconnect / clear) → reject. Fail-closed.
        rx.await.unwrap_or(false)
    }
}
```

- [ ] **Step 3: Change `send_msg` + `WsSink` to use the writer half**

Replace the existing `send_msg` and `WsSink` definitions with:

```rust
/// Send one `ServerMessage` as a JSON text frame through the writer half.
async fn send_msg(writer: &mut WsWriter, msg: &ServerMessage) -> anyhow::Result<()> {
    let json = serde_json::to_string(msg)?;
    writer.send(Message::Text(json.into())).await?;
    Ok(())
}

/// A sink that writes each event to the socket's writer half as a `ServerMessage::Event` frame.
struct WsSink<'a> {
    writer: &'a mut WsWriter,
}

#[async_trait::async_trait]
impl EventSink for WsSink<'_> {
    async fn emit(&mut self, event: &Event) -> anyhow::Result<()> {
        send_msg(
            self.writer,
            &ServerMessage::Event {
                event: event.clone(),
            },
        )
        .await
    }
}
```

- [ ] **Step 4: Rewrite `handle_socket` to read concurrently with the turn**

Replace the entire `handle_socket` function body with:

```rust
async fn handle_socket(socket: WebSocket, params: ConnectParams, state: Arc<ServeState>) {
    // Split up-front so the turn (writer) and inbound approvals (reader) can run concurrently.
    let (mut writer, mut reader) = socket.split();

    let session = match resolve_session(&params, &state).await {
        Ok(s) => s,
        Err(e) => {
            let _ = send_msg(
                &mut writer,
                &ServerMessage::Error {
                    message: e.to_string(),
                },
            )
            .await;
            return;
        }
    };

    if send_msg(
        &mut writer,
        &ServerMessage::Ready {
            session,
            capabilities: state.capabilities.clone(),
        },
    )
    .await
    .is_err()
    {
        return;
    }

    // Reconnect: replay the gap after `last_seq`.
    if let Some(after) = params.last_seq {
        match state.service.store().replay_since(session, Some(after)).await {
            Ok(events) => {
                for event in events {
                    if send_msg(&mut writer, &ServerMessage::Event { event })
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
            }
            Err(e) => {
                let _ = send_msg(
                    &mut writer,
                    &ServerMessage::Error {
                        message: e.to_string(),
                    },
                )
                .await;
                return;
            }
        }
    }

    let approvals = ApprovalRegistry::new();

    'outer: while let Some(Ok(msg)) = reader.next().await {
        let text = match msg {
            Message::Text(t) => t,
            Message::Close(_) => break,
            _ => continue, // ignore binary/ping/pong
        };
        let command: Command = match serde_json::from_str(text.as_str()) {
            Ok(c) => c,
            Err(e) => {
                let _ = send_msg(
                    &mut writer,
                    &ServerMessage::Error {
                        message: format!("bad command: {e}"),
                    },
                )
                .await;
                continue;
            }
        };
        match command {
            Command::SendPrompt { text, .. } => {
                let approver = Arc::new(InteractiveApprover::new(approvals.clone()));
                // Drive the turn while concurrently reading inbound approvals. The turn borrows
                // `writer` (via the sink); the reader borrows `reader` — disjoint, so `select!`
                // can poll both. `StreamExt::next` is cancel-safe, so the reader future being
                // dropped when the turn wins a race loses no inbound frame.
                let turn_err = {
                    let mut sink = WsSink { writer: &mut writer };
                    let turn = state
                        .service
                        .run_prompt_with_approver(session, &text, &mut sink, approver);
                    tokio::pin!(turn);
                    let mut err: Option<anyhow::Error> = None;
                    loop {
                        tokio::select! {
                            res = &mut turn => {
                                if let Err(e) = res {
                                    err = Some(e);
                                }
                                approvals.clear();
                                break;
                            }
                            inbound = reader.next() => match inbound {
                                Some(Ok(Message::Text(t))) => {
                                    match serde_json::from_str::<Command>(t.as_str()) {
                                        Ok(Command::ApproveDiff { id, approved, .. }) => {
                                            approvals.resolve(id, approved);
                                        }
                                        Ok(Command::Abort { .. }) => {
                                            let _ = state.service.abort(session).await;
                                            approvals.clear();
                                            break 'outer;
                                        }
                                        // A second SendPrompt mid-turn is ignored (one turn at a
                                        // time); other commands are no-ops here.
                                        _ => {}
                                    }
                                }
                                Some(Ok(Message::Close(_))) | None => {
                                    approvals.clear();
                                    break 'outer;
                                }
                                _ => {}
                            }
                        }
                    }
                    err
                }; // `sink` dropped here → `writer` is free again

                if let Some(e) = turn_err {
                    let _ = send_msg(
                        &mut writer,
                        &ServerMessage::Error {
                            message: e.to_string(),
                        },
                    )
                    .await;
                }
            }
            Command::Abort { .. } => {
                let _ = state.service.abort(session).await;
                break;
            }
            Command::ApproveDiff { .. } => {
                // No turn in flight: a stray approval is ignored.
            }
            Command::CreateSession => {
                // The session is already established on connect; nothing to do.
            }
        }
    }
}
```

- [ ] **Step 5: Run the existing serve + workspace tests**

Run: `cargo test -p otto-engine --test serve && cargo test -p otto-engine --test cors --test remote_workspace`
Expected: PASS — streaming, replay, auth, TLS, CORS, and the `/workspace` RPC all behave as before (the writer-half refactor is transparent; approval is off because these servers use `build_tool_registry`).

- [ ] **Step 6: Commit**

```bash
cargo fmt --all
git add crates/engine/Cargo.toml crates/engine/src/serve.rs
git commit -m "feat(engine): serve reads socket concurrently with the turn; interactive edit approval"
```

---

## Task 7: serve — approval integration tests (approve / reject / disconnect)

**Files:**
- Modify: `crates/engine/tests/serve.rs` (add an approval-mode server helper + three tests)

- [ ] **Step 1: Add an approval-mode server helper**

In `crates/engine/tests/serve.rs`, add `build_tool_registry_approving` to the `otto_engine` import:

```rust
use otto_engine::{
    EngineService, build_default_registry, build_tool_registry, build_tool_registry_approving,
    serve_app, serve_run,
};
```

Add a helper that returns the port **and** the workspace dir path (so tests can check the on-disk file). Keep the tempdir alive via the returned `TempDir`:

```rust
/// Start a serve app in **approval mode** (ordinary writes gated `Ask`). Returns the bound
/// port and the tempdir (whose path is the workspace root the Coder edits: `out.txt`).
async fn start_approval_server() -> (u16, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let provider = ScriptedProvider::new("{}")
        .on(
            "edits",
            r#"{"edits": [{"path": "out.txt", "contents": "approved contents"}]}"#,
        )
        .on("milestones", r#"{"milestones": [{"description": "x"}]}"#);
    let router: Arc<dyn otto_engine_core::Router> =
        Arc::new(SingleProviderRouter::new(Arc::new(provider)));
    let workspace: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(dir.path()));
    let tools_ws: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(dir.path()));
    let tools = Arc::new(build_tool_registry_approving(
        tools_ws,
        dir.path().to_path_buf(),
    ));
    let store: Arc<dyn otto_persistence::SessionStore> = Arc::new(
        otto_persistence::SqliteStore::open(dir.path().join("s.db"))
            .await
            .unwrap(),
    );
    let service = EngineService::new(
        store,
        Arc::new(build_default_registry()),
        router,
        workspace,
        tools,
    );
    let app = serve_app(service, TOKEN.to_string(), test_capabilities());
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        serve_run(listener, app, None).await.unwrap();
    });
    (port, dir)
}

/// Read frames until an `ApprovalRequest` event arrives; return its `id`. Panics on TurnComplete
/// or stream end first (means no approval was requested).
async fn next_approval_id(
    ws: &mut (impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin),
) -> String {
    while let Some(frame) = next_json_opt(ws).await {
        if frame["type"] == "event" {
            let kind = &frame["event"]["kind"];
            if let Some(req) = kind.get("ApprovalRequest") {
                return req["id"].as_str().unwrap().to_string();
            }
            if kind.get("TurnComplete").is_some() {
                panic!("turn completed before any ApprovalRequest");
            }
        }
    }
    panic!("stream ended before any ApprovalRequest");
}
```

- [ ] **Step 2: Add the approve test**

```rust
#[tokio::test]
async fn approved_edit_is_written() {
    let (port, dir) = start_approval_server().await;
    let (mut ws, _) = tokio_tungstenite::connect_async(authed_request(port, ""))
        .await
        .expect("connect");
    let ready: Value = next_json(&mut ws).await;
    let session = ready["session"].as_str().unwrap().to_string();

    let cmd = serde_json::json!({ "SendPrompt": { "session": session, "text": "add a greeting" } });
    ws.send(Message::Text(serde_json::to_string(&cmd).unwrap()))
        .await
        .unwrap();

    // Approve every ApprovalRequest until the turn completes (the repair loop may re-propose).
    let id = next_approval_id(&mut ws).await;
    let approve =
        serde_json::json!({ "ApproveDiff": { "session": session, "id": id, "approved": true } });
    ws.send(Message::Text(serde_json::to_string(&approve).unwrap()))
        .await
        .unwrap();

    let mut saw_turn_complete = false;
    while let Some(frame) = next_json_opt(&mut ws).await {
        if frame["type"] == "event" {
            let kind = &frame["event"]["kind"];
            if let Some(req) = kind.get("ApprovalRequest") {
                let id = req["id"].as_str().unwrap().to_string();
                let a = serde_json::json!({ "ApproveDiff": { "session": session, "id": id, "approved": true } });
                ws.send(Message::Text(serde_json::to_string(&a).unwrap()))
                    .await
                    .unwrap();
            } else if kind.get("TurnComplete").is_some() {
                saw_turn_complete = true;
                break;
            }
        }
    }
    assert!(saw_turn_complete);
    let written = std::fs::read_to_string(dir.path().join("out.txt")).expect("out.txt written");
    assert_eq!(written, "approved contents");
}
```

- [ ] **Step 3: Add the reject test**

```rust
#[tokio::test]
async fn rejected_edit_is_not_written() {
    let (port, dir) = start_approval_server().await;
    let (mut ws, _) = tokio_tungstenite::connect_async(authed_request(port, ""))
        .await
        .expect("connect");
    let ready: Value = next_json(&mut ws).await;
    let session = ready["session"].as_str().unwrap().to_string();

    let cmd = serde_json::json!({ "SendPrompt": { "session": session, "text": "add a greeting" } });
    ws.send(Message::Text(serde_json::to_string(&cmd).unwrap()))
        .await
        .unwrap();

    // Reject every ApprovalRequest until the turn completes.
    let mut saw_turn_complete = false;
    while let Some(frame) = next_json_opt(&mut ws).await {
        if frame["type"] == "event" {
            let kind = &frame["event"]["kind"];
            if let Some(req) = kind.get("ApprovalRequest") {
                let id = req["id"].as_str().unwrap().to_string();
                let r = serde_json::json!({ "ApproveDiff": { "session": session, "id": id, "approved": false } });
                ws.send(Message::Text(serde_json::to_string(&r).unwrap()))
                    .await
                    .unwrap();
            } else if kind.get("TurnComplete").is_some() {
                saw_turn_complete = true;
                break;
            }
        }
    }
    assert!(saw_turn_complete);
    assert!(
        !dir.path().join("out.txt").exists(),
        "a rejected edit must not be written"
    );
}
```

- [ ] **Step 4: Add the disconnect-mid-approval test**

```rust
#[tokio::test]
async fn disconnect_mid_approval_fails_closed() {
    let (port, dir) = start_approval_server().await;
    let (mut ws, _) = tokio_tungstenite::connect_async(authed_request(port, ""))
        .await
        .expect("connect");
    let ready: Value = next_json(&mut ws).await;
    let session = ready["session"].as_str().unwrap().to_string();

    let cmd = serde_json::json!({ "SendPrompt": { "session": session, "text": "add a greeting" } });
    ws.send(Message::Text(serde_json::to_string(&cmd).unwrap()))
        .await
        .unwrap();

    // Wait until the server is blocked on an approval, then drop the socket without replying.
    let _id = next_approval_id(&mut ws).await;
    drop(ws);

    // The edit is only ever written on approval; a disconnect resolves the pending request to
    // false (fail-closed), so out.txt must never appear. Give the server a moment to settle.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(
        !dir.path().join("out.txt").exists(),
        "a disconnect mid-approval must not write the edit"
    );
}
```

- [ ] **Step 5: Run the approval tests**

Run: `cargo test -p otto-engine --test serve`
Expected: PASS — all existing serve tests plus the three new approval tests.

- [ ] **Step 6: Commit**

```bash
cargo fmt --all
git add crates/engine/tests/serve.rs
git commit -m "test(engine): serve approval — approve writes, reject/disconnect fail closed"
```

---

## Task 8: CLI — `otto serve --approve-edits`

**Files:**
- Modify: `crates/engine/src/main.rs` (parse the flag; thread `approve_edits` through `build_tools_preferring_mcp`)

- [ ] **Step 1: Thread `approve_edits` into `build_tools_preferring_mcp`**

In `crates/engine/src/main.rs`, change the function signature and the registry construction:

```rust
async fn build_tools_preferring_mcp(
    tools_workspace: Arc<dyn Workspace>,
    root: PathBuf,
    approve_edits: bool,
) -> (ToolRegistry, Vec<McpConnection>) {
    let mut registry = if approve_edits {
        otto_engine::build_tool_registry_approving(tools_workspace, root.clone())
    } else {
        build_tool_registry(tools_workspace, root.clone())
    };
    let mut conns = Vec::new();
    // … rest unchanged …
```

- [ ] **Step 2: Update the `cmd_run` call site (approval off)**

In `cmd_run`, the call becomes:

```rust
    let (tools, _mcp_conns) =
        build_tools_preferring_mcp(tools_workspace, root.clone(), false).await;
```

- [ ] **Step 3: Parse `--approve-edits` in `cmd_serve` and pass it**

In `cmd_serve`, add a flag local and a parse arm. After `let mut tls_key: Option<PathBuf> = None;` add:

```rust
    let mut approve_edits = false;
```

In the `match a.as_str()` arg loop, add an arm (before the `_ => {}`):

```rust
            "--approve-edits" => approve_edits = true,
```

Change the tools construction in `cmd_serve` to:

```rust
    let (tools, _mcp_conns) =
        build_tools_preferring_mcp(tools_workspace, root.clone(), approve_edits).await;
```

Update the usage strings (lines ~2 and ~26) to mention the flag:

```rust
//! `otto serve [--root <path>] [--port <p>] [--approve-edits]` — serve over WebSocket (needs OTTO_TOKEN).
```

and

```rust
                "usage:\n  otto run \"<goal>\" [--root <path>]\n  otto serve [--root <path>] [--port <p>] [--approve-edits]"
```

- [ ] **Step 4: Verify it builds**

Run: `cargo build -p otto-engine`
Expected: clean build.

- [ ] **Step 5: Manual smoke check (optional, not a test)**

```bash
OTTO_TOKEN=dev cargo run -p otto-engine -- serve --approve-edits --port 7878
```
Expected: prints `otto serve listening on ws://127.0.0.1:7878/ws`. Ctrl-C to stop.

- [ ] **Step 6: Commit**

```bash
cargo fmt --all
git add crates/engine/src/main.rs
git commit -m "feat(engine): otto serve --approve-edits flag enables interactive edit approval"
```

---

## Task 9: UI — `diff_lines` + `ApprovalRequest` log row

**Files:**
- Modify: `ui/src/view_model.rs` (add `DiffKind`/`DiffLine`/`diff_lines` + tests; add the `describe_event` arm)

Run all UI commands from inside `ui/`.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `ui/src/view_model.rs`:

```rust
    #[test]
    fn diff_new_file_is_all_adds() {
        let d = diff_lines(None, "a\nb\n");
        assert_eq!(d.len(), 2);
        assert!(d.iter().all(|l| l.kind == DiffKind::Add));
        assert_eq!(d[0].text, "a");
        assert_eq!(d[1].text, "b");
    }

    #[test]
    fn diff_identical_is_all_context() {
        let d = diff_lines(Some("a\nb\n"), "a\nb\n");
        assert!(d.iter().all(|l| l.kind == DiffKind::Context));
        assert_eq!(d.len(), 2);
    }

    #[test]
    fn diff_middle_change_keeps_context_head_and_tail() {
        let d = diff_lines(Some("a\nB\nc\n"), "a\nX\nc\n");
        // a = context, B = del, X = add, c = context
        assert_eq!(d[0].kind, DiffKind::Context);
        assert_eq!(d[0].text, "a");
        assert_eq!(d[1].kind, DiffKind::Del);
        assert_eq!(d[1].text, "B");
        assert_eq!(d[2].kind, DiffKind::Add);
        assert_eq!(d[2].text, "X");
        assert_eq!(d[3].kind, DiffKind::Context);
        assert_eq!(d[3].text, "c");
    }

    #[test]
    fn diff_pure_append() {
        let d = diff_lines(Some("a\n"), "a\nb\n");
        assert_eq!(d[0].kind, DiffKind::Context);
        assert_eq!(d[1].kind, DiffKind::Add);
        assert_eq!(d[1].text, "b");
    }

    #[test]
    fn describe_approval_request_row() {
        let r = describe_event(&EventKind::ApprovalRequest {
            id: uuid::Uuid::from_u128(0),
            path: PathBuf::from("src/main.rs"),
            old: None,
            new: "x".into(),
        });
        assert_eq!(r.class, "row-approval");
        assert!(r.text.contains("src/main.rs"));
    }
```

The `describe_approval_request_row` test needs `uuid` in scope; `uuid` is already a `ui` dependency. Add `use otto_protocol::EventKind;` is already present at module top.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cd ui && cargo test diff_ && cargo test describe_approval`
Expected: FAIL — `cannot find function diff_lines` / non-exhaustive `describe_event` match.

- [ ] **Step 3: Add the diff types + function**

In `ui/src/view_model.rs`, add near the top (after the existing `LogRow` definitions):

```rust
/// The role of a line in a rendered diff.
#[derive(Clone, PartialEq, Debug)]
pub enum DiffKind {
    Context,
    Add,
    Del,
}

/// One line in a rendered diff.
#[derive(Clone, PartialEq, Debug)]
pub struct DiffLine {
    pub kind: DiffKind,
    pub text: String,
}

/// Line diff of `old` → `new`: a common prefix and suffix render as `Context`; the divergent
/// middle renders as `Del` (old) then `Add` (new). `old == None` means a new file (all `Add`).
/// A minimal, dependency-free diff sufficient for the diff-first approval surface.
pub fn diff_lines(old: Option<&str>, new: &str) -> Vec<DiffLine> {
    let old_lines: Vec<&str> = old.map(|s| s.lines().collect()).unwrap_or_default();
    let new_lines: Vec<&str> = new.lines().collect();

    // Common prefix.
    let mut start = 0;
    while start < old_lines.len()
        && start < new_lines.len()
        && old_lines[start] == new_lines[start]
    {
        start += 1;
    }
    // Common suffix (not overlapping the prefix).
    let mut end_old = old_lines.len();
    let mut end_new = new_lines.len();
    while end_old > start && end_new > start && old_lines[end_old - 1] == new_lines[end_new - 1] {
        end_old -= 1;
        end_new -= 1;
    }

    let mut out = Vec::new();
    for line in &old_lines[..start] {
        out.push(DiffLine { kind: DiffKind::Context, text: (*line).to_string() });
    }
    for line in &old_lines[start..end_old] {
        out.push(DiffLine { kind: DiffKind::Del, text: (*line).to_string() });
    }
    for line in &new_lines[start..end_new] {
        out.push(DiffLine { kind: DiffKind::Add, text: (*line).to_string() });
    }
    for line in &new_lines[end_new..] {
        out.push(DiffLine { kind: DiffKind::Context, text: (*line).to_string() });
    }
    out
}
```

- [ ] **Step 4: Add the `describe_event` arm**

In `describe_event`, add an arm (before the closing `}` of the match):

```rust
        EventKind::ApprovalRequest { path, .. } => row(
            "row-approval",
            format!("⏸ approval needed: {}", path.display()),
        ),
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cd ui && cargo test`
Expected: PASS — all host-side UI tests including the new diff + approval-row tests.

- [ ] **Step 6: Commit**

```bash
cargo fmt --all
git add ui/src/view_model.rs
git commit -m "feat(ui): diff_lines pure diff + ApprovalRequest log row"
```

---

## Task 10: UI — ApprovalPanel component + app wiring

**Files:**
- Create: `ui/src/components/approval_panel.rs`
- Modify: `ui/src/components/mod.rs` (module + re-export)
- Modify: `ui/src/app.rs` (pending state, set on `ApprovalRequest`, send `ApproveDiff`, render panel)

This component is verified by the WASM build compiling (the existing UI components follow the same "verified by wasm compile + manual browser testing" convention — see `ui/src/ws.rs`). The behavior logic it relies on (`diff_lines`) is already host-tested in Task 9.

- [ ] **Step 1: Create the ApprovalPanel component**

Create `ui/src/components/approval_panel.rs`:

```rust
use std::path::PathBuf;

use leptos::prelude::*;
use uuid::Uuid;

use crate::view_model::{diff_lines, DiffKind};

/// A pending edit approval surfaced to the user: the correlation id, the path, and the diff
/// inputs (old contents — `None` for a new file — and the proposed contents).
pub type PendingApproval = (Uuid, PathBuf, Option<String>, String);

/// Renders the pending diff (if any) with Approve / Reject buttons. `on_decide` is called with
/// the approval id and the verdict.
#[component]
pub fn ApprovalPanel(
    pending: Signal<Option<PendingApproval>>,
    on_decide: Callback<(Uuid, bool)>,
) -> impl IntoView {
    move || {
        pending.get().map(|(id, path, old, new)| {
            let lines = diff_lines(old.as_deref(), &new);
            let rows = lines
                .into_iter()
                .map(|l| {
                    let cls = match l.kind {
                        DiffKind::Add => "diff-add",
                        DiffKind::Del => "diff-del",
                        DiffKind::Context => "diff-ctx",
                    };
                    view! { <div class=cls>{l.text}</div> }
                })
                .collect_view();
            view! {
                <div class="approval">
                    <div class="approval-head">
                        {format!("Approve edit to {}?", path.display())}
                    </div>
                    <div class="approval-diff">{rows}</div>
                    <div class="approval-actions">
                        <button
                            class="approve-btn"
                            on:click=move |_| on_decide.run((id, true))
                        >
                            "Approve"
                        </button>
                        <button
                            class="reject-btn"
                            on:click=move |_| on_decide.run((id, false))
                        >
                            "Reject"
                        </button>
                    </div>
                </div>
            }
        })
    }
}
```

- [ ] **Step 2: Register the component**

In `ui/src/components/mod.rs`, add:

```rust
mod approval_panel;
```

and to the `pub use` block:

```rust
pub use approval_panel::{ApprovalPanel, PendingApproval};
```

- [ ] **Step 3: Wire app state — pending signal, set on event, send verdict, render**

In `ui/src/app.rs`:

1. Add imports — extend the `components` use and add `EventKind` + `PendingApproval`:

```rust
use crate::components::{
    ApprovalPanel, ConnectionForm, EditorPane, EventLog, FileTree, PromptBar, StatusLine,
};
use otto_protocol::{CapabilitiesManifest, Command, EventKind, ServerMessage, SessionId};
```

2. Add a pending-approval signal alongside the other state signals (after `editor_dirty`):

```rust
    let pending_approval = RwSignal::new(None::<crate::components::PendingApproval>);
```

3. In the `on_msg` closure's `Ok(ServerMessage::Event { event })` arm, set the pending approval when the event is an `ApprovalRequest`, before pushing the row:

```rust
            Ok(ServerMessage::Event { event }) => {
                if should_apply(last_seq.get_untracked(), event.seq) {
                    last_seq.set(advance_last_seq(last_seq.get_untracked(), event.seq));
                    if let EventKind::ApprovalRequest { id, path, old, new } = &event.kind {
                        pending_approval.set(Some((
                            *id,
                            path.clone(),
                            old.clone(),
                            new.clone(),
                        )));
                    }
                    rows.update(|v| v.push(describe_event(&event.kind)));
                }
            }
```

4. Clear the pending approval on every disconnect path. In `connect` (right after `capabilities.set(None);`), in `disconnect`, in `on_close`, and in `on_error`, add:

```rust
        pending_approval.set(None);
```

5. Add a `decide` callback (near `abort`):

```rust
    let decide = move |(id, approved): (Uuid, bool)| {
        let (Some(ws), Some(sid)) = (socket.get(), session.get()) else {
            return;
        };
        let Ok(uuid) = Uuid::parse_str(&sid) else {
            return;
        };
        let cmd = Command::ApproveDiff {
            session: SessionId(uuid),
            id,
            approved,
        };
        if let Err(e) = send_command(&ws, &cmd) {
            rows.update(|v| v.push(client_error_row(&e)));
        }
        pending_approval.set(None);
    };
```

6. Render the panel — add it just above `<EventLog rows=rows />` in the `view!`:

```rust
            <ApprovalPanel
                pending=pending_approval.into()
                on_decide=Callback::new(decide)
            />
```

- [ ] **Step 4: Verify the WASM build compiles**

Run: `cd ui && cargo build --target wasm32-unknown-unknown`
Expected: clean build (this is the component's verification, per the UI convention).

Also re-run host tests to ensure nothing regressed:

Run: `cd ui && cargo test`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
git add ui/src/components/approval_panel.rs ui/src/components/mod.rs ui/src/app.rs
git commit -m "feat(ui): ApprovalPanel + wire ApprovalRequest → diff + ApproveDiff"
```

---

## Task 11: Docs — mark D shipped

**Files:**
- Modify: `docs/superpowers/specs/2026-06-17-ui-roadmap.md` (status line + row D)
- Modify: `CLAUDE.md` (the UI paragraph)

- [ ] **Step 1: Update the roadmap**

In `docs/superpowers/specs/2026-06-17-ui-roadmap.md`:

- Change the `**Status:**` line to note D shipped, e.g. append `; D: diff approval, 2026-06-19 — [design](2026-06-19-ui-diff-approval-design.md) · [plan](../plans/2026-06-19-ui-diff-approval.md); E–F pending.` and adjust the existing `D–F pending` wording to `E–F pending`.
- In the sub-projects table, change row `**D**` to `**D** ✅` and append to its "Protocol / engine changes" cell a `**Done:**` note: the `ApproveDiff` command + `ApprovalRequest` event; the async `Approver` seam + `DenyApprover`; the orchestrator's `Ask` branch routing to it; the opt-in `serve --approve-edits` gate (`ApprovalModeGate`); and serve's concurrent socket (`split` + `select!`) with a per-connection `ApprovalRegistry`/`InteractiveApprover`.

- [ ] **Step 2: Update CLAUDE.md**

In `CLAUDE.md`, in the UI paragraph (the one ending with sub-projects D–F pending), add a sentence recording that **sub-project D shipped** (diff approval): the opt-in `otto serve --approve-edits` flag gates ordinary `fs.write` as `Ask`; the orchestrator emits an `ApprovalRequest` and applies the edit only on an explicit `ApproveDiff`; serve now reads the socket concurrently with the running turn; the UI renders the diff with Approve/Reject. Note E–F remain pending. Keep it to 2–3 sentences in the existing style.

- [ ] **Step 3: Verify the whole workspace + UI still pass**

Run: `cargo test --workspace && cd ui && cargo test && cd ..`
Expected: PASS across the board.

- [ ] **Step 4: Commit**

```bash
git add docs/superpowers/specs/2026-06-17-ui-roadmap.md CLAUDE.md
git commit -m "docs: record sub-project D (diff approval) shipped"
```

---

## Final verification (after all tasks)

- [ ] `cargo fmt --all -- --check` — clean
- [ ] `cargo clippy --workspace --all-targets` — no new warnings
- [ ] `cargo test --workspace` — green (offline, deterministic; default `serve`/`run` unchanged)
- [ ] `cd ui && cargo test && cargo build --target wasm32-unknown-unknown` — green
- [ ] Spec coverage check: `ApproveDiff`/`ApprovalRequest` (T1) · `Approver`/`DenyApprover` (T2) · orchestrator `Ask` routing (T3) · `ApprovalModeGate` + approving registry (T4) · `run_prompt_with_approver` (T5) · concurrent serve + `InteractiveApprover` (T6) · approve/reject/disconnect integration (T7) · `--approve-edits` flag (T8) · `diff_lines` + log row (T9) · ApprovalPanel + app wiring (T10) · docs (T11). All spec requirements mapped.
