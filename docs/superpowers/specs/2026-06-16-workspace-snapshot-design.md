# otto Design — Workspace Snapshot

**Status:** approved design (spec). Implementation plan to follow in `docs/superpowers/plans/`.
**Date:** 2026-06-16

## Goal

Give the `Workspace` seam a `snapshot()` that captures the workspace's current files as a
transferable `WorkspaceSnapshot`, plus a `LocalWorkspace::restore` that materializes one into a
fresh workspace. This is the first sub-project of the **remote axis** and the piece deferred
from persistence Plan C: it is the prerequisite for promote-to-remote (transfer the working
state to a remote engine) and for `RemoteWorkspace` reconstitution. Pure-local and fully
offline-testable.

## Context

The architecture's `Workspace` seam shows `async fn snapshot(&self) -> Result<WorkspaceSnapshot>`,
but only `apply_edit` is implemented today. `LocalWorkspace` is plain-filesystem (no git): it
has `read`, `list` (a recursive `**` walk that skips `target`/`.git`/`node_modules`/dotfiles),
and `apply_edit` (which path-contains every write under the root). This sub-project implements
the missing `snapshot` and a restore counterpart.

## Decisions (locked during brainstorming)

1. **Content model = full-content bundle, not a git diff.** A snapshot is every file
   `list("**")` returns, each captured as `path → bytes`. The architecture's phrase
   "uncommitted diffs" implies a git diff against a base, but `LocalWorkspace` has no git
   (git integration lives in the future `mcp-git` crate). A full-content bundle is self-
   contained and restorable into any empty directory — sufficient for handover. A smaller
   git-diff/patch-bundle is a future refinement, deferred with git integration.
2. **`WorkspaceSnapshot` lives in `engine-core`** (where the `Workspace` trait is defined),
   serde-serializable so it can later cross the wire.
3. **`snapshot` goes on the `Workspace` trait** (matching the architecture); **`restore` is an
   inherent `LocalWorkspace` method**, not a trait method — the trait stays minimal, and
   `RemoteWorkspace` will reconstitute its own way.
4. **Restore writes through `apply_edit`**, so every file in a (possibly untrusted) bundle
   passes the same path-containment guard — a `../` entry cannot escape the root.

## Architecture

### The type (`crates/engine-core/src/types.rs`)

```rust
/// A transferable capture of a workspace's current files (relative path -> contents).
/// Excludes the same paths `list("**")` excludes (`target`/`.git`/`node_modules`/dotfiles).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceSnapshot {
    pub files: Vec<(PathBuf, Vec<u8>)>,
}
```

### The seam (`crates/engine-core/src/traits.rs`)

```rust
#[async_trait]
pub trait Workspace: WorkspaceRead {
    async fn apply_edit(&self, edit: &Edit) -> anyhow::Result<u64>;
    /// Capture the workspace's current files for handover. Excludes ignored dirs.
    async fn snapshot(&self) -> anyhow::Result<WorkspaceSnapshot>;
}
```

Adding a trait method touches every `Workspace` impl. The only production impl is
`LocalWorkspace`; test doubles (e.g. `RecordingWorkspace` in the orchestrator tests) also
implement `Workspace` and gain a trivial `snapshot` (empty or recorded). The plan enumerates
each impl site.

### `LocalWorkspace::snapshot` + `restore` (`crates/workspace/src/lib.rs`)

- `snapshot()`: `self.list("**")` → for each path, `self.read(path)` → collect
  `(path, bytes)`. Reuses the existing recursive walk (ignored dirs already excluded) and the
  path-contained read. A file that vanishes mid-snapshot surfaces as an error (fail-loud — a
  snapshot is a consistency capture, unlike `list`'s best-effort per-entry skip).
- `restore(&self, snapshot)`: inherent method; for each `(path, bytes)` in the snapshot, build
  an `Edit { path, new_contents }` and call `self.apply_edit(&edit)`. Path containment is
  enforced by `apply_edit` (`contain`), so a malicious `../` path in a bundle is rejected.
  (`Edit.new_contents` is a `String`; see "Open detail" below.)

### Open detail — `Edit.new_contents` is `String`, snapshot is `Vec<u8>`

`apply_edit` takes an `Edit` whose `new_contents` is a `String`. To restore arbitrary bytes
through `apply_edit`, restore converts bytes via `String::from_utf8`. For v1 the snapshot
targets text workspaces (the same files the agents read/edit), so restore requires valid UTF-8
and errors on non-UTF-8 content rather than corrupting it. This keeps restore on the single
gated write path (`apply_edit`) rather than introducing a second raw-bytes write path. A
raw-bytes write path for true binary support is a future refinement, noted as a deferral.

## Error handling & determinism

- Pure-local, no network. Round-trips are deterministic (`tempfile` in tests).
- `snapshot` is fail-loud on a read error (consistency capture).
- `restore` is fail-closed on a path-escape (via `apply_edit`) and on non-UTF-8 content.

## Testing

- **Round-trip:** build a workspace with nested files (`src/lib.rs`, `src/inner/mod.rs`,
  `a.txt`) plus ignored dirs (`target/junk`, `.git/config`, `node_modules/x`); `snapshot()`;
  `restore()` into a fresh empty `LocalWorkspace`; assert the restored files match the
  originals and the ignored entries are absent from the snapshot.
- **Exclusions:** assert `snapshot().files` contains no path under `target`/`.git`/
  `node_modules`/a dotfile.
- **Path-escape guard:** hand `restore` a `WorkspaceSnapshot` with a `../escape.txt` entry and
  assert it errors (no write outside the root).
- **Non-UTF-8:** a snapshot file with invalid UTF-8 bytes makes `restore` error (documents the
  v1 text-only restore boundary).

## Out of scope (named, not silently dropped)

- **Git diff / patch-bundle** (smaller, diff-vs-base snapshots) — arrives with git integration
  (`mcp-git`).
- **Wiring `WorkspaceSnapshot` into `SessionState` / the promote flow** — the promote
  sub-project; this one delivers only the `Workspace` seam.
- **Raw-bytes (binary) restore** — restore is UTF-8-only for v1 (single gated write path).
- **Snapshot size cap** — no cap yet (mirrors the existing unbounded-bash-output deferral).
