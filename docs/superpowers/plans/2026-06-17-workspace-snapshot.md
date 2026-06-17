# Workspace Snapshot Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add `Workspace::snapshot() -> WorkspaceSnapshot` (a full-content capture of the workspace's files) and an inherent `LocalWorkspace::restore` that materializes a snapshot through the gated `apply_edit` path.

**Architecture:** `WorkspaceSnapshot` is a serde-serializable `Vec<(PathBuf, Vec<u8>)>` in `engine-core`. `LocalWorkspace::snapshot` reuses `list("**")` + `read` (so it already excludes `target`/`.git`/`node_modules`/dotfiles). `restore` writes each file via `apply_edit`, inheriting its path-containment guard; it is UTF-8-only for v1 (`Edit.new_contents` is a `String`). Adding the trait method touches both `Workspace` impls (`LocalWorkspace`, the `RecordingWorkspace` test double).

**Tech Stack:** Rust (edition 2024), `serde` (added to engine-core), `tokio`/`tempfile` for tests.

**Spec:** `docs/superpowers/specs/2026-06-16-workspace-snapshot-design.md` (deferrals: git-diff bundle, promote wiring, binary restore, size cap).

---

### Task 1: `WorkspaceSnapshot` type + serde

**Files:**
- Modify: `crates/engine-core/Cargo.toml`
- Modify: `crates/engine-core/src/types.rs`

- [ ] **Step 1: Add the `serde` dependency**

In `crates/engine-core/Cargo.toml`, under `[dependencies]`, add (after `serde_json.workspace = true`):

```toml
serde = { workspace = true }
```

- [ ] **Step 2: Write the failing serde round-trip test**

In `crates/engine-core/src/types.rs`, add at the end of the file:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_snapshot_round_trips_through_json() {
        let snap = WorkspaceSnapshot {
            files: vec![
                (PathBuf::from("a.txt"), b"hello".to_vec()),
                (PathBuf::from("src/lib.rs"), vec![0, 1, 2, 255]),
            ],
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: WorkspaceSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snap, back);
    }
}
```

- [ ] **Step 3: Run the test to verify it fails**

Run: `cargo test -p otto-engine-core types::tests::workspace_snapshot_round_trips_through_json`
Expected: FAIL to compile — `WorkspaceSnapshot` does not exist.

- [ ] **Step 4: Add the type**

In `crates/engine-core/src/types.rs`, add the serde import at the top (below `use std::path::PathBuf;`):

```rust
use serde::{Deserialize, Serialize};
```

Add the type (place it after the `Edit` struct):

```rust
/// A transferable capture of a workspace's current files (relative path -> contents).
/// Excludes the same paths `list("**")` excludes (`target`/`.git`/`node_modules`/dotfiles).
/// Serde-serializable so it can later cross the wire to a remote engine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceSnapshot {
    pub files: Vec<(PathBuf, Vec<u8>)>,
}
```

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo test -p otto-engine-core types::tests::workspace_snapshot_round_trips_through_json`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/engine-core/Cargo.toml crates/engine-core/src/types.rs Cargo.lock
git commit -m "feat(engine-core): WorkspaceSnapshot type"
```

---

### Task 2: `Workspace::snapshot` — trait method + impls

**Files:**
- Modify: `crates/engine-core/src/traits.rs`
- Modify: `crates/engine-core/src/orchestrator.rs` (the `RecordingWorkspace` test double)
- Modify: `crates/workspace/src/lib.rs`

- [ ] **Step 1: Write the failing `LocalWorkspace::snapshot` test**

In `crates/workspace/src/lib.rs`, change the top import line:

```rust
use otto_engine_core::types::Edit;
```

to:

```rust
use otto_engine_core::types::{Edit, WorkspaceSnapshot};
```

Add this test to the `#[cfg(test)] mod tests` block (the helpers/imports for `Edit`, `PathBuf`, `LocalWorkspace` are already in scope via `use super::*;`):

```rust
    #[tokio::test]
    async fn snapshot_captures_listed_files_and_excludes_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let ws = LocalWorkspace::new(dir.path());
        for (p, c) in [
            ("a.txt", "A"),
            ("src/lib.rs", "L"),
            ("src/inner/mod.rs", "M"),
            ("target/junk.rs", "x"),
            (".git/config", "x"),
            ("node_modules/x/i.js", "x"),
        ] {
            ws.apply_edit(&Edit {
                path: PathBuf::from(p),
                new_contents: c.to_string(),
            })
            .await
            .unwrap();
        }

        let snap = ws.snapshot().await.unwrap();
        let paths: Vec<_> = snap.files.iter().map(|(p, _)| p.clone()).collect();
        assert!(paths.contains(&PathBuf::from("a.txt")));
        assert!(paths.contains(&PathBuf::from("src/lib.rs")));
        assert!(paths.contains(&PathBuf::from("src/inner/mod.rs")));
        assert!(!paths.iter().any(|p| p.starts_with("target")));
        assert!(!paths.iter().any(|p| p.starts_with(".git")));
        assert!(!paths.iter().any(|p| p.starts_with("node_modules")));
        // Contents are captured, not just paths.
        let lib = snap
            .files
            .iter()
            .find(|(p, _)| p == &PathBuf::from("src/lib.rs"))
            .unwrap();
        assert_eq!(lib.1, b"L");
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p otto-workspace snapshot_captures_listed_files_and_excludes_ignored`
Expected: FAIL to compile — `snapshot` is not a method on `Workspace`.

- [ ] **Step 3: Add `snapshot` to the `Workspace` trait**

In `crates/engine-core/src/traits.rs`, add the method to the `Workspace` trait (after `apply_edit`):

```rust
    /// Capture the workspace's current files as a transferable snapshot, for handover.
    /// Excludes the same paths `list` excludes. (`RemoteWorkspace` reconstitutes from this.)
    async fn snapshot(&self) -> anyhow::Result<WorkspaceSnapshot>;
```

Ensure `WorkspaceSnapshot` is in scope in `traits.rs`. The file already imports from `crate::types` (the `use crate::types::{...}` line near the top) — add `WorkspaceSnapshot` to that import list. (It currently imports `{AgentOutput, AgentRequest, CompleteRequest, CompleteResponse, Edit}`; make it `{AgentOutput, AgentRequest, CompleteRequest, CompleteResponse, Edit, WorkspaceSnapshot}`.)

- [ ] **Step 4: Implement the trivial `snapshot` for the `RecordingWorkspace` test double**

In `crates/engine-core/src/orchestrator.rs`, in the `#[cfg(test)] mod tests` block, find `impl Workspace for RecordingWorkspace` and add a `snapshot` method to it (after its `apply_edit`):

```rust
        async fn snapshot(&self) -> anyhow::Result<crate::types::WorkspaceSnapshot> {
            Ok(crate::types::WorkspaceSnapshot { files: Vec::new() })
        }
```

- [ ] **Step 5: Implement `LocalWorkspace::snapshot`**

In `crates/workspace/src/lib.rs`, in `impl Workspace for LocalWorkspace`, add (after `apply_edit`):

```rust
    async fn snapshot(&self) -> anyhow::Result<WorkspaceSnapshot> {
        let paths = self.list("**").await?;
        let mut files = Vec::with_capacity(paths.len());
        for path in paths {
            let bytes = self.read(&path).await?;
            files.push((path, bytes));
        }
        Ok(WorkspaceSnapshot { files })
    }
```

- [ ] **Step 6: Run the test to verify it passes**

Run: `cargo test -p otto-workspace snapshot_captures_listed_files_and_excludes_ignored`
Expected: PASS.

- [ ] **Step 7: Confirm engine-core still compiles and tests pass**

Run: `cargo test -p otto-engine-core`
Expected: PASS — the `RecordingWorkspace` double implements the new method, so the orchestrator tests still build and pass.

- [ ] **Step 8: Commit**

```bash
git add crates/engine-core/src/traits.rs crates/engine-core/src/orchestrator.rs crates/workspace/src/lib.rs
git commit -m "feat(workspace): Workspace::snapshot captures the listed files"
```

---

### Task 3: `LocalWorkspace::restore`

**Files:**
- Modify: `crates/workspace/src/lib.rs`

- [ ] **Step 1: Write the failing tests**

In `crates/workspace/src/lib.rs`, add to `mod tests`:

```rust
    #[tokio::test]
    async fn snapshot_restore_round_trips_into_fresh_workspace() {
        let src_dir = tempfile::tempdir().unwrap();
        let src = LocalWorkspace::new(src_dir.path());
        for (p, c) in [("a.txt", "A"), ("src/lib.rs", "L"), ("src/inner/mod.rs", "M")] {
            src.apply_edit(&Edit {
                path: PathBuf::from(p),
                new_contents: c.to_string(),
            })
            .await
            .unwrap();
        }
        let snap = src.snapshot().await.unwrap();

        let dst_dir = tempfile::tempdir().unwrap();
        let dst = LocalWorkspace::new(dst_dir.path());
        dst.restore(&snap).await.unwrap();

        // Re-snapshotting the destination yields the same files + contents.
        let mut original = snap.files.clone();
        original.sort();
        let mut restored = dst.snapshot().await.unwrap().files;
        restored.sort();
        assert_eq!(original, restored);
    }

    #[tokio::test]
    async fn restore_rejects_path_escape() {
        let dir = tempfile::tempdir().unwrap();
        let ws = LocalWorkspace::new(dir.path());
        let snap = WorkspaceSnapshot {
            files: vec![(PathBuf::from("../escape.txt"), b"x".to_vec())],
        };
        assert!(ws.restore(&snap).await.is_err());
    }

    #[tokio::test]
    async fn restore_rejects_non_utf8() {
        let dir = tempfile::tempdir().unwrap();
        let ws = LocalWorkspace::new(dir.path());
        let snap = WorkspaceSnapshot {
            files: vec![(PathBuf::from("bin.dat"), vec![0xff, 0xfe])],
        };
        assert!(ws.restore(&snap).await.is_err());
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p otto-workspace restore`
Expected: FAIL to compile — `restore` is not a method on `LocalWorkspace`.

- [ ] **Step 3: Implement `restore`**

In `crates/workspace/src/lib.rs`, add an inherent method to the `impl LocalWorkspace { ... }` block (the one that already has `new`/`contain`):

```rust
    /// Materialize a snapshot into this workspace, writing each file through the gated
    /// `apply_edit` path (so path containment is enforced). UTF-8 only for v1: a non-UTF-8
    /// file errors rather than corrupting (raw-bytes restore is a future refinement).
    pub async fn restore(&self, snapshot: &WorkspaceSnapshot) -> anyhow::Result<()> {
        for (path, bytes) in &snapshot.files {
            let new_contents = String::from_utf8(bytes.clone()).map_err(|_| {
                anyhow::anyhow!("restore: non-UTF-8 contents for {}", path.display())
            })?;
            self.apply_edit(&Edit {
                path: path.clone(),
                new_contents,
            })
            .await?;
        }
        Ok(())
    }
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p otto-workspace restore`
Expected: PASS (3 tests).

- [ ] **Step 5: Commit**

```bash
git add crates/workspace/src/lib.rs
git commit -m "feat(workspace): LocalWorkspace::restore materializes a snapshot"
```

---

### Task 4: Full workspace verification

**Files:** none (verification only)

- [ ] **Step 1: Format, lint, and test the whole workspace**

Run: `cargo fmt --all && cargo clippy --workspace --all-targets && cargo test --workspace`
Expected: fmt clean (or trivial changes you then include), clippy clean across the workspace, all tests green — the new `engine-core`/`workspace` tests plus every existing crate unchanged (the `RecordingWorkspace` double gained `snapshot`, so the orchestrator tests still build).

- [ ] **Step 2: If `cargo fmt` changed anything, commit it**

```bash
git add -A
git commit -m "style: cargo fmt after workspace snapshot"
```

(If fmt made no changes, skip this commit.)

---

## Done criteria

- `WorkspaceSnapshot { files: Vec<(PathBuf, Vec<u8>)> }` in `engine-core`, serde round-trips.
- `Workspace::snapshot()` on the trait; `LocalWorkspace::snapshot` captures the listed files (excluding ignored dirs) with contents; the `RecordingWorkspace` double implements it.
- `LocalWorkspace::restore` writes a snapshot through `apply_edit` (path-contained); round-trip is faithful; path-escape and non-UTF-8 entries error.
- `cargo test --workspace` green; clippy/fmt clean.

**Next in the remote axis:** TLS/WSS for serve, then `RemoteWorkspace` + a workspace RPC, then `RemoteTarget`/promote (where this `WorkspaceSnapshot` gets wired into the handover).
