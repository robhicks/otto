# otto Plan 4a — Gated Edit Application Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close the ungated-filesystem-write gap before LLM-backed coder agents arrive: run every Coder edit through the permission gate (the sensitive-path floor) before the orchestrator applies it, so a (future, possibly prompt-injected) Coder cannot write `.env`/`.ssh`/`.git` files.

**Architecture:** The orchestrator applies a Coder's `AgentOutput::Code { edits }` directly via `workspace.apply_edit` — bypassing the `PermissionGate` that governs tool calls. This plan adds `ToolRegistry::check(name, args) -> Decision` (gate decision without dispatch) and has the orchestrator consult it for `fs.write` on each edit path before applying; a `Deny` is logged and the edit skipped. Small, fully offline-testable, and a documented decision on the second ungated path (`ctx.workspace()` stays a trusted accessor for built-in agents).

**Tech Stack:** Rust (edition 2024), serde_json, async-trait, anyhow.

---

## Context for the implementer (read once)

This is the security prerequisite for Plan 4's LLM agents (tracked in project memory: "two ungated FS paths must be governed before LLM-backed coder agents ship").

Current state (`main`):
- `engine-core::tool::ToolRegistry` has private fields `{ tools, gate, ask }` and methods `new(gate, ask)`, `register(tool)`, `call(name, args)` (runs the gate then dispatches). `Decision { Allow, Ask, Deny }`. `PermissionGate::evaluate(tool, args) -> Decision`.
- `engine-core::orchestrator::Orchestrator { registry, router, workspace, tools }`. In `run_turn`, the Execute phase applies the Coder's edits:
  ```rust
  for edit in &edits {
      let bytes_written = self.workspace.apply_edit(edit).await?;
      emit.emit(EventKind::FileEdit { path: edit.path.clone(), bytes_written });
  }
  ```
  This is the ungated path being fixed.
- `DefaultPermissionGate` (otto-tools) denies sensitive paths in `path`/`paths`/`glob` args (case-insensitive). The engine wires it into the registry; for `fs.write` with a sensitive `path`, it returns `Deny`.

**The two ungated paths (memory-tracked):**
1. **Orchestrator `apply_edit`** — FIXED by this plan (gate-check each edit path).
2. **Public `ctx.workspace()` accessor** — DOCUMENTED DECISION: it stays a trusted accessor (only built-in agents reach it; the real Coder returns edits, it does not write directly). This plan documents that; a read-only workspace view for untrusted agents is a later refinement.

**Conventions:** stay on branch `feat/plan-4a-gated-edits`; never detach HEAD; `git add`+`commit` only (no `--amend`); no AI/Claude self-attribution; per-package then workspace gates; `clippy -D warnings` clean; TDD.

---

## Task 1: `ToolRegistry::check` — gate decision without dispatch

**Files:**
- Modify: `crates/engine-core/src/tool.rs`

- [ ] **Step 1: Add the `check` method**

In `crates/engine-core/src/tool.rs`, add a `check` method to the `impl ToolRegistry` block (after `register`, before `call`):

```rust
    /// Return the gate's `Decision` for a proposed call WITHOUT dispatching. Lets the
    /// orchestrator gate edits it applies directly (via the workspace) through the same
    /// policy that governs tool calls.
    pub fn check(&self, name: &str, args: &Value) -> Decision {
        self.gate.evaluate(name, args)
    }
```

- [ ] **Step 2: Add a test**

In the `#[cfg(test)] mod tests` of `tool.rs`, add (the test module already has `AllowAll`/`DenyAll` gates from earlier tasks and `use serde_json::json;`):

```rust
    #[test]
    fn check_returns_gate_decision_without_dispatch() {
        let allow = ToolRegistry::new(Arc::new(AllowAll), Arc::new(DenyAsk));
        assert_eq!(allow.check("fs.write", &json!({"path": "a.txt"})), Decision::Allow);

        let deny = ToolRegistry::new(Arc::new(DenyAll), Arc::new(DenyAsk));
        assert_eq!(deny.check("fs.write", &json!({"path": "a.txt"})), Decision::Deny);
    }
```

(If the existing gate doubles in the test module are named differently, use whatever `AllowAll`/`DenyAll`-equivalent `PermissionGate` test impls exist; the point is one gate that returns `Allow` and one that returns `Deny`. If only `AllowAll` exists, add a `DenyAll` returning `Decision::Deny`.)

- [ ] **Step 3: Test**

Run: `cargo test -p otto-engine-core tool::` (the new test passes), `cargo clippy -p otto-engine-core --all-targets -- -D warnings` (clean), `cargo fmt -p otto-engine-core` (clean), `cargo test --workspace` (all pass — purely additive).

- [ ] **Step 4: Commit**

```bash
git add crates/engine-core/src/tool.rs
git commit -m "feat(engine-core): ToolRegistry::check exposes the gate decision without dispatch"
```

---

## Task 2: Orchestrator gates Coder edits before applying

**Files:**
- Modify: `crates/engine-core/src/orchestrator.rs`

- [ ] **Step 1: Gate each edit in the Execute phase**

In `crates/engine-core/src/orchestrator.rs`, change the imports so `Decision` is available. The current line is `use crate::tool::ToolRegistry;`. Change it to:

```rust
use crate::tool::{Decision, ToolRegistry};
```

Replace the edit-application loop (currently lines ~109-115):

```rust
        for edit in &edits {
            let bytes_written = self.workspace.apply_edit(edit).await?;
            emit.emit(EventKind::FileEdit {
                path: edit.path.clone(),
                bytes_written,
            });
        }
```

with a gated version (the gate is consulted for `fs.write` on each edit's path; a `Deny` is logged and the edit skipped, never applied):

```rust
        for edit in &edits {
            let check_args = serde_json::json!({ "path": edit.path.to_string_lossy() });
            if self.tools.check("fs.write", &check_args) == Decision::Deny {
                emit.emit(EventKind::Log {
                    message: format!(
                        "edit to {} denied by permission gate; skipped",
                        edit.path.display()
                    ),
                });
                continue;
            }
            let bytes_written = self.workspace.apply_edit(edit).await?;
            emit.emit(EventKind::FileEdit {
                path: edit.path.clone(),
                bytes_written,
            });
        }
```

(`serde_json` is already a dependency of engine-core. `serde_json::json!` is referenced fully-qualified here to avoid touching the file's import list beyond `Decision`.)

- [ ] **Step 2: Run the existing tests (happy path unchanged)**

Run: `cargo test -p otto-engine-core orchestrator::`
Expected: `run_turn_drives_full_spine_and_emits_ordered_events` STILL passes — the test's `empty_tools()` uses `TestAllowGate` (returns `Allow`), so `check("fs.write", ...)` is not `Deny`, the edit applies, and the 12-element ordered-event vector is unchanged. `run_turn_errors_when_a_role_is_missing` still passes.

- [ ] **Step 3: Add a deny-path test**

In the `#[cfg(test)] mod tests` of `orchestrator.rs`, add a gate that denies `fs.write` and a test proving the edit is skipped + logged. After the `TestAllowGate` definition, add:

```rust
    struct TestDenyWriteGate;
    impl PermissionGate for TestDenyWriteGate {
        fn evaluate(&self, tool: &str, _args: &Value) -> Decision {
            if tool == "fs.write" {
                Decision::Deny
            } else {
                Decision::Allow
            }
        }
    }
    fn deny_write_tools() -> ToolRegistry {
        ToolRegistry::new(Arc::new(TestDenyWriteGate), Arc::new(DenyAsk))
    }
```

Then add the test:

```rust
    #[tokio::test]
    async fn denied_edit_is_skipped_and_logged() {
        let reg = registry(); // OneEditCoder produces an edit to out.txt
        let router = FakeRouter;
        let workspace = RecordingWorkspace::default();
        let tools = deny_write_tools();
        let orch = Orchestrator {
            registry: &reg,
            router: &router,
            workspace: &workspace,
            tools: &tools,
        };

        let events: Arc<Mutex<Vec<EventKind>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = {
            let events = Arc::clone(&events);
            move |kind: EventKind| events.lock().unwrap().push(kind)
        };

        let outcome = orch.run_turn(SessionId::new(), "x", &sink).await.unwrap();

        // The turn still completes; the Verifier is unaffected.
        assert_eq!(outcome, TurnOutcome { ok: true });
        // The edit was NOT applied to the workspace.
        assert_eq!(workspace.edits.lock().unwrap().len(), 0);

        let recorded = events.lock().unwrap().clone();
        // A denial Log was emitted, and NO FileEdit event for out.txt.
        assert!(recorded.iter().any(|e| matches!(
            e,
            EventKind::Log { message } if message.contains("denied by permission gate")
        )));
        assert!(!recorded.iter().any(|e| matches!(e, EventKind::FileEdit { .. })));
    }
```

- [ ] **Step 4: Test**

Run: `cargo test -p otto-engine-core` (all orchestrator + tool tests pass, including `denied_edit_is_skipped_and_logged`), `cargo clippy -p otto-engine-core --all-targets -- -D warnings` (clean), `cargo fmt -p otto-engine-core` (clean), `cargo test --workspace` (all pass — the engine's real turn uses `DefaultPermissionGate`, which allows ordinary paths like `otto_output.txt`, so the integration test is unaffected).

- [ ] **Step 5: Commit**

```bash
git add crates/engine-core/src/orchestrator.rs
git commit -m "feat(engine-core): gate Coder edits through the permission gate before applying"
```

---

## Task 3: Docs + quality gate

**Files:**
- Modify: `docs/ARCHITECTURE.md`

- [ ] **Step 1: Document the gated-edit path**

In `docs/ARCHITECTURE.md`, find the `### \`Tool\` — the tool-call seam (+ the permission gate)` subsection. Append a sentence to it:

```markdown
The orchestrator also runs every Coder edit through `ToolRegistry::check("fs.write", {path})`
(the same gate, without dispatch) before applying it via the workspace, so a Coder cannot
write a sensitive path — a denied edit is logged and skipped. The `ctx.workspace()` accessor
remains a trusted, direct handle for built-in agents (the real Coder returns edits rather than
writing directly); a read-only workspace view for untrusted agents is a later refinement.
```

- [ ] **Step 2: Final gate**

Run: `cargo fmt --all -- --check` (clean), `cargo clippy --workspace --all-targets -- -D warnings` (clean), `cargo test --workspace` (all pass — capture the total).

- [ ] **Step 3: Commit**

```bash
git add docs/ARCHITECTURE.md
git commit -m "docs: document gated Coder-edit application"
```

---

## Done — what Plan 4a delivers

Coder edits now pass the permission gate before the orchestrator applies them — the sensitive-path floor (`.env`/`.ssh`/`.git`/`.aws`/ssh-keys) now governs writes, not just tool calls. A denied edit is logged and skipped rather than silently written. This closes the higher-stakes of the two memory-tracked ungated paths; the second (`ctx.workspace()`) is a documented trusted accessor pending a read-only-view refinement.

**This is the security prerequisite for the real LLM agents.** Next: Plan 4b — real LLM-backed Planner + Coder (prompt the router, parse structured output), then 4c — real Verifier (runs `cargo test`/build via the sandboxed `bash` tool) + the orchestrator Repair loop (retry on verify failure, increment `RouteHints.prior_failures`), then 4d — real ContextFinder + retrieval (AST + git + grep).
