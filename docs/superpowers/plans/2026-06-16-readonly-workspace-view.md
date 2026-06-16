# Read-Only Workspace View Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close the latent ungated-write bypass in `AgentCtx` by splitting `Workspace` into a read-only supertrait (`WorkspaceRead`) and a writable `Workspace`, and exposing only `&dyn WorkspaceRead` to agents.

**Architecture:** `WorkspaceRead { read, list }` becomes a supertrait; `Workspace: WorkspaceRead { apply_edit }`. `AgentCtx::workspace()` returns `&dyn WorkspaceRead`, so an agent calling `apply_edit` through it is a compile error. The orchestrator and the `fs.*` tools keep the full `&dyn Workspace`/`Arc<dyn Workspace>`; `&dyn Workspace` upcasts implicitly to `&dyn WorkspaceRead` (stable since Rust 1.86; toolchain is 1.95), so the only edits are the trait definition, `AgentCtx`, and the two concrete impls (`LocalWorkspace` + the `RecordingWorkspace` test double).

**Tech Stack:** Rust (edition 2024, toolchain stable 1.95), async-trait, anyhow, tokio, tempfile (dev).

---

## Context for the implementer (read once)

- This is a focused refactor of a trait seam. The split is **atomic**: once `Workspace`'s methods move to `WorkspaceRead`, every concrete impl and `AgentCtx` must be updated together or the workspace won't compile. So Task 1 is one cohesive change verified by a green build + test suite (the compiler enforces the read-only guarantee; a positive test proves the view still reads).
- Why this matters: today `AgentCtx::workspace()` hands every agent a `&dyn Workspace` whose `apply_edit` is an **ungated write** (it bypasses the permission gate and the orchestrator's Allow-gated apply). No agent uses it, but the seam permits it. The split removes the write capability from the agent-facing view at the type level.
- Only **two** concrete `Workspace` impls exist: `LocalWorkspace` (`crates/workspace/src/lib.rs`) and the test double `RecordingWorkspace` (`crates/engine-core/src/orchestrator.rs` test module). Everything else holds `&dyn Workspace` / `Arc<dyn Workspace>` and is unaffected (supertrait methods `read`/`list` remain callable on `dyn Workspace`, and `&dyn Workspace` upcasts to `&dyn WorkspaceRead` at `AgentCtx::new` call sites).
- Agent test call sites pass `&LocalWorkspace`, which coerces directly to `&dyn WorkspaceRead` (no change).
- Conventions: branch `feat/readonly-workspace-view`; never detach HEAD; `git add`+`commit` only (no `--amend`); `cargo clippy --workspace --all-targets -- -D warnings` clean; `cargo fmt` clean; **no AI/Claude self-attribution anywhere**.

## Execution setup (before Task 1)

```bash
git checkout main && git checkout -b feat/readonly-workspace-view
```

---

## File Structure

```
crates/engine-core/src/traits.rs        # MODIFY: split Workspace -> WorkspaceRead + Workspace; AgentCtx exposes WorkspaceRead
crates/engine-core/src/lib.rs           # MODIFY: export WorkspaceRead
crates/workspace/src/lib.rs             # MODIFY: split LocalWorkspace's impl into WorkspaceRead + Workspace
crates/engine-core/src/orchestrator.rs  # MODIFY: split the RecordingWorkspace test double's impl + import
crates/agents/src/lib.rs                # MODIFY: add a test proving AgentCtx exposes a working read-only view
docs/ARCHITECTURE.md                     # MODIFY (Task 2): document the read-only view
```

---

## Task 1: Split the Workspace trait and narrow AgentCtx

**Files:**
- Modify: `crates/engine-core/src/traits.rs`, `crates/engine-core/src/lib.rs`, `crates/workspace/src/lib.rs`, `crates/engine-core/src/orchestrator.rs`, `crates/agents/src/lib.rs`

- [ ] **Step 1: Split the trait in `crates/engine-core/src/traits.rs`**

Replace the existing `Workspace` trait definition (the `#[async_trait] pub trait Workspace { read; list; apply_edit }` block at lines ~20-28) with:

```rust
/// Read access to the repository the engine operates on. This is the agent-facing view
/// (`AgentCtx::workspace()`) — agents may read, but cannot mutate, the workspace.
#[async_trait]
pub trait WorkspaceRead: Send + Sync {
    async fn read(&self, path: &Path) -> anyhow::Result<Vec<u8>>;
    async fn list(&self, glob: &str) -> anyhow::Result<Vec<PathBuf>>;
}

/// The writable repository. `LocalWorkspace` edits a real folder in place (no clone);
/// `RemoteWorkspace` operates on a remote checkout (later plan). Only the orchestrator and the
/// gated `fs.write` tool hold this; agents get the read-only `WorkspaceRead` view.
#[async_trait]
pub trait Workspace: WorkspaceRead {
    /// Apply a full-file edit, returning the number of bytes written.
    async fn apply_edit(&self, edit: &Edit) -> anyhow::Result<u64>;
}
```

- [ ] **Step 2: Narrow `AgentCtx` to the read-only view (same file)**

In `crates/engine-core/src/traits.rs`, change the `AgentCtx` field, the `new` parameter, and the accessor from `Workspace` to `WorkspaceRead`:

Change the field (line ~41):
```rust
    workspace: &'a dyn WorkspaceRead,
```
Change the `new` parameter (line ~48):
```rust
        workspace: &'a dyn WorkspaceRead,
```
Change the accessor (lines ~63-66):
```rust
    /// The read-only workspace view agents may read from. Writes are NOT available here — they
    /// go through the gated `fs.write` tool or the orchestrator's gated apply.
    pub fn workspace(&self) -> &dyn WorkspaceRead {
        self.workspace
    }
```

- [ ] **Step 3: Export `WorkspaceRead` from `crates/engine-core/src/lib.rs`**

Change the traits re-export line (currently `pub use traits::{Agent, AgentCtx, Provider, Workspace};`) to:
```rust
pub use traits::{Agent, AgentCtx, Provider, Workspace, WorkspaceRead};
```

- [ ] **Step 4: Split `LocalWorkspace`'s impl in `crates/workspace/src/lib.rs`**

Change the import at the top (line ~6) from:
```rust
use otto_engine_core::traits::Workspace;
```
to:
```rust
use otto_engine_core::traits::{Workspace, WorkspaceRead};
```

Then split the single `impl Workspace for LocalWorkspace { read; list; apply_edit }` block into two impls. Replace `#[async_trait]\nimpl Workspace for LocalWorkspace {` (the line before `async fn read`) so that `read` and `list` live under `WorkspaceRead`, and `apply_edit` lives under `Workspace`. Concretely:

- Keep the `async fn read(...)` and `async fn list(...)` method bodies exactly as they are, but under `impl WorkspaceRead for LocalWorkspace`.
- Move `async fn apply_edit(...)` (body unchanged) into a separate `impl Workspace for LocalWorkspace`.

The result should look like:
```rust
#[async_trait]
impl WorkspaceRead for LocalWorkspace {
    async fn read(&self, path: &Path) -> anyhow::Result<Vec<u8>> {
        // ... existing body unchanged ...
    }

    async fn list(&self, glob: &str) -> anyhow::Result<Vec<PathBuf>> {
        // ... existing body unchanged (shallow + recursive walk) ...
    }
}

#[async_trait]
impl Workspace for LocalWorkspace {
    async fn apply_edit(&self, edit: &Edit) -> anyhow::Result<u64> {
        // ... existing body unchanged ...
    }
}
```
(Do not change any method body — only which `impl` block each method sits in, and add the second `#[async_trait]` attribute on the new `impl Workspace` block.)

- [ ] **Step 5: Split the `RecordingWorkspace` test double in `crates/engine-core/src/orchestrator.rs`**

In the test module, change the import (line ~184) from:
```rust
    use crate::traits::{Agent, Workspace};
```
to:
```rust
    use crate::traits::{Agent, Workspace, WorkspaceRead};
```

Then split its impl (lines ~245-257). Replace:
```rust
    #[async_trait]
    impl Workspace for RecordingWorkspace {
        async fn read(&self, _path: &Path) -> anyhow::Result<Vec<u8>> {
            Ok(Vec::new())
        }
        async fn list(&self, _glob: &str) -> anyhow::Result<Vec<PathBuf>> {
            Ok(Vec::new())
        }
        async fn apply_edit(&self, edit: &Edit) -> anyhow::Result<u64> {
            self.edits.lock().unwrap().push(edit.clone());
            Ok(edit.new_contents.len() as u64)
        }
    }
```
with:
```rust
    #[async_trait]
    impl WorkspaceRead for RecordingWorkspace {
        async fn read(&self, _path: &Path) -> anyhow::Result<Vec<u8>> {
            Ok(Vec::new())
        }
        async fn list(&self, _glob: &str) -> anyhow::Result<Vec<PathBuf>> {
            Ok(Vec::new())
        }
    }
    #[async_trait]
    impl Workspace for RecordingWorkspace {
        async fn apply_edit(&self, edit: &Edit) -> anyhow::Result<u64> {
            self.edits.lock().unwrap().push(edit.clone());
            Ok(edit.new_contents.len() as u64)
        }
    }
```

NOTE: the non-test orchestrator code keeps `use crate::traits::{AgentCtx, Workspace};` (line 9) and the `workspace: &'a dyn Workspace` field — unchanged. `AgentCtx::new(self.router, self.workspace, self.tools)` at line ~47 compiles because `&dyn Workspace` upcasts to `&dyn WorkspaceRead`.

- [ ] **Step 6: Add a test proving the read-only view works (in `crates/agents/src/lib.rs`)**

Append this test module to `crates/agents/src/lib.rs` (the crate has `otto-providers`, `otto-router`, `otto-tools`, `otto-workspace`, and `tempfile` as dev-dependencies — confirmed by the existing agent test modules):

```rust
#[cfg(test)]
mod readonly_view_tests {
    use otto_engine_core::tool::{DenyAsk, ToolRegistry};
    use otto_engine_core::traits::{AgentCtx, Workspace, WorkspaceRead};
    use otto_engine_core::types::Edit;
    use otto_providers::LocalProvider;
    use otto_router::SingleProviderRouter;
    use otto_tools::DefaultPermissionGate;
    use otto_workspace::LocalWorkspace;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    #[tokio::test]
    async fn agentctx_exposes_a_working_readonly_workspace_view() {
        let dir = tempfile::tempdir().unwrap();
        let ws = LocalWorkspace::new(dir.path());
        ws.apply_edit(&Edit {
            path: PathBuf::from("seed.txt"),
            new_contents: "hi".to_string(),
        })
        .await
        .unwrap();

        let router = SingleProviderRouter::new(Arc::new(LocalProvider::new()));
        let tools = ToolRegistry::new(Arc::new(DefaultPermissionGate::new()), Arc::new(DenyAsk));
        let ctx = AgentCtx::new(&router, &ws, &tools);

        // The agent-facing view is read-only and can read.
        let view: &dyn WorkspaceRead = ctx.workspace();
        let bytes = view.read(Path::new("seed.txt")).await.unwrap();
        assert_eq!(bytes, b"hi");
        // `view` has no `apply_edit` method — the ungated write path is gone at the type level
        // (enforced by the compiler; this annotation documents the read-only type).
    }
}
```

- [ ] **Step 7: Build, test, lint, format**

Run, in order:
- `cargo build --workspace` — must compile. (If `&dyn Workspace`→`&dyn WorkspaceRead` upcasting errors appear, confirm the toolchain is stable ≥ 1.86 via `rustc --version`; it is 1.95 here.)
- `cargo test --workspace` — ALL pass, including the new `agentctx_exposes_a_working_readonly_workspace_view` and the existing 97. The orchestrator tests using `RecordingWorkspace` still pass (it now impls both traits).
- `cargo clippy --workspace --all-targets -- -D warnings` — clean.
- `cargo fmt --all -- --check` — clean.

- [ ] **Step 8: Commit**

```bash
git add crates/engine-core/src/traits.rs crates/engine-core/src/lib.rs crates/workspace/src/lib.rs crates/engine-core/src/orchestrator.rs crates/agents/src/lib.rs
git commit -m "feat(engine-core): read-only workspace view for agents (WorkspaceRead supertrait)"
```

---

## Task 2: Docs + final quality gate

**Files:**
- Modify: `docs/ARCHITECTURE.md`

- [ ] **Step 1: Document the read-only view**

In `docs/ARCHITECTURE.md`, find the text describing `AgentCtx` / the `Workspace` seam (it describes `AgentCtx` granting scoped access to `router()`, `workspace()`, and `tools()`, and/or lists the `Workspace` trait). Update it to reflect the split — add or weave in:

```markdown
The workspace seam is split into `WorkspaceRead` (`read`, `list`) and `Workspace: WorkspaceRead`
(adds `apply_edit`). `AgentCtx::workspace()` exposes only the read-only `WorkspaceRead` view, so
an agent cannot mutate the workspace directly — writes flow exclusively through the gated
`fs.write` tool and the orchestrator's permission-gated apply. The orchestrator and the `fs.*`
tools hold the full `Workspace`.
```

Reconcile any neighboring sentence that says agents "read from / write edits to" the workspace (they now only read through this view). Do not make unrelated edits.

- [ ] **Step 2: Final gate**

Run and capture output:
- `cargo fmt --all -- --check` (clean)
- `cargo clippy --workspace --all-targets -- -D warnings` (clean)
- `cargo test --workspace` — capture per-crate `test result:` lines + summed total.

- [ ] **Step 3: Commit**

```bash
git add docs/ARCHITECTURE.md
git commit -m "docs: document the read-only workspace view"
```

---

## Done — what this delivers

Agents receive a read-only `WorkspaceRead` view through `AgentCtx`; the ungated `apply_edit`
write path is removed from the agent-facing surface at compile time. Writes remain available
only to the orchestrator and the gated `fs.write` tool, which hold the full `Workspace`. The
seam stays remote-ready (a future `RemoteWorkspace` implements the same two traits).

**Carried forward / deferred:**
- The `fs.read`/`fs.list` tools still hold `Arc<dyn Workspace>` rather than `Arc<dyn WorkspaceRead>` — a cosmetic narrowing (they're gate-mediated and engine-constructed, not handed to agents).
- Reads through the read-only view are raw (the sensitive-path floor remains a tool-layer concern; no agent reads via the view today).
- No per-agent trust levels — every agent gets the read-only view uniformly.
```
