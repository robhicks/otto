# Snapshot Sensitive-Floor Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Apply the sensitive-path floor inside `LocalWorkspace::snapshot`, so a floor-sensitive file can never leave an engine inside a `Workspace::snapshot` — closing the promote-push secret-egress path in #127 at the seam every caller shares.

**Architecture:** One filter, at one seam. `promote()` is left untouched so it *inherits* the fix; filtering there instead would fix today's caller and leave the next one to reintroduce the gap.

**Tech Stack:** Rust (edition 2024, toolchain pinned 1.97.0), `tokio` + `tempfile` for tests. **No new dependency** — `crates/workspace` already depends on `otto-protocol`, which exports `is_sensitive`.

**Spec:** `docs/superpowers/specs/2026-08-01-snapshot-sensitive-floor-design.md` — read it first. This plan implements it exactly.

## Global Constraints

- **The floor is not widened.** `SENSITIVE_MARKERS` is untouched. This change *applies* the existing floor in one more place — a tightening, never a relaxation.
- ~~**No new dependency** in any crate.~~ **Amended during implementation:** `otto-workspace` was
  added to `crates/remote`'s **`[dev-dependencies]`**. Step 6 requires a `remote` test that fails
  when the guard is removed, and that needs the real `LocalWorkspace` — `crates/remote`'s test
  module had never exercised `promote()` against a real workspace, so a stub would have asserted
  only the stub's own behavior and Step 7's mutation would have exposed it as worthless. Dev-only,
  no cycle (`otto-workspace` has no path back to `otto-remote`), production graph unchanged. The
  constraint's intent — do not grow the shipped dependency graph — is preserved.
- **`promote()` in `crates/remote` is not modified.** It must inherit the filtering. If a task seems to need a change there, stop — that is the rejected alternative in the spec.
- **`list()` behavior is unchanged** (only its false comment is corrected). Filtering `list` would perturb the ContextFinder for no security gain here.
- **`EngineService::filtered_workspace_snapshot` stays.** It filters through `tools.check("fs.read", …)`, which is strictly broader than the floor — it also honours `PolicyGate` rules. The two compose.
- Determinism holds: no env read, no network. `ui-dioxus/` untouched.
- No AI attribution in any commit message, comment, or doc.
- Run `cargo fmt --all` from the repo root before every Rust commit.
- CI merge gate, and the definition of done: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings` (**any warning blocks merge**), `cargo test --workspace -- --skip rust_analyzer_integration`.

## File Structure

| File | Responsibility |
|---|---|
| `crates/workspace/src/lib.rs:148-156` | **Modify.** `snapshot` skips paths where `otto_protocol::is_sensitive`. |
| `crates/workspace/src/lib.rs:84-87` | **Modify.** The walk's comment claims the dotfile skip "also covers the gate's sensitive-path floor". It does not — that false claim is why the raw snapshot was trusted. |
| `crates/workspace/src/lib.rs` (tests) | **Modify.** Regression test asserting no `is_sensitive` path survives a snapshot. |
| `crates/remote/src/lib.rs` (tests) | **Modify.** Assert `promote()`'s bundle inherits the filtering. |
| `CLAUDE.md` | **Modify.** The `workspace` crate row states that `snapshot()` applies the floor; the `remote` row notes the dev-only `otto-workspace` edge. |
| `crates/workspace/src/remote.rs` | **Modify** *(added during review).* `RemoteWorkspace::snapshot` re-applies the floor instead of trusting the peer. |
| `crates/engine-core/src/{traits.rs,types.rs}` | **Modify** *(added during review).* Doc-only: the same false dotfile-covers-the-floor claim lived at the seam contract, one level up. |
| `crates/remote/Cargo.toml` | **Modify** *(added during review).* The dev-dependency above. |

## Task Order & Rationale

> **What actually shipped: three commits, not one.** Review after Task 1 found the seam's contract
> was satisfied by *delegation* — `RemoteWorkspace::snapshot` trusted the peer to be an up-to-date
> otto that filters — which is the same shape of assumption that caused this bug. A second commit
> enforces the floor locally there too, via a shared `strip_sensitive_files` helper with its own
> test. A third applies a review nit (`retain` in place rather than filter-and-collect). The
> File Structure table above is annotated with the files those added.

One task. The change is ~6 lines; splitting it would produce a commit that adds a test for behavior the next commit introduces. Both tests land with the fix so the branch is never in a state where the guarantee is claimed but unenforced.

---

### Task 1: Apply the floor in `snapshot`, and pin it with two tests

**Files:**
- Modify: `crates/workspace/src/lib.rs` (impl + comment + test)
- Modify: `crates/remote/src/lib.rs` (test)
- Modify: `CLAUDE.md`

**Interfaces:**
- Consumes: `otto_protocol::is_sensitive` (already a dependency, already public).
- Produces: no signature change anywhere. `Workspace::snapshot`'s contract narrows — it now never returns a floor-sensitive path.

- [ ] **Step 1: Write the failing test**

Add to `crates/workspace/src/lib.rs`'s existing `#[cfg(test)] mod tests`:

```rust
    /// The walk skips dotfiles, which is NOT the same as the sensitive-path floor: the markers
    /// match as substrings, so `id_rsa` and `production.env` have no leading dot and sail
    /// through. A snapshot reads whole file *contents* for shipment off-machine (it is what
    /// `otto_remote::promote` puts in a `PromoteBundle`), so the floor has to be re-asserted
    /// here. Measured before the fix: the snapshot contained `id_rsa`, `production.env`, and
    /// `config/local.env`.
    #[tokio::test]
    async fn snapshot_excludes_floor_sensitive_files_that_are_not_dotfiles() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("id_rsa"), b"PRIVATE KEY").unwrap();
        std::fs::write(dir.path().join("production.env"), b"DB_PASSWORD=hunter2").unwrap();
        std::fs::create_dir_all(dir.path().join("config")).unwrap();
        std::fs::write(dir.path().join("config/local.env"), b"SECRET=xyz").unwrap();
        std::fs::write(dir.path().join(".env"), b"HIDDEN=1").unwrap();
        std::fs::write(dir.path().join("ok.txt"), b"fine").unwrap();

        let ws = LocalWorkspace::new(dir.path());
        let snap = ws.snapshot().await.unwrap();
        let names: Vec<String> = snap
            .files
            .iter()
            .map(|(p, _)| p.to_string_lossy().to_string())
            .collect();

        // Asserted against `is_sensitive` rather than a hardcoded list, so the test tracks the
        // floor automatically if a marker is ever added.
        let leaked: Vec<&String> = names
            .iter()
            .filter(|n| otto_protocol::is_sensitive(n))
            .collect();
        assert!(leaked.is_empty(), "floor-sensitive files in a snapshot: {leaked:?}");

        // ...and the snapshot is not vacuously empty.
        assert!(
            names.iter().any(|n| n == "ok.txt"),
            "ordinary files must still be captured: {names:?}"
        );
    }
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p otto-workspace snapshot_excludes_floor_sensitive`
Expected: FAIL — `floor-sensitive files in a snapshot: ["config/local.env", "id_rsa", "production.env"]`

- [ ] **Step 3: Apply the floor in `snapshot`**

In `crates/workspace/src/lib.rs`, replace the body of `snapshot` (`:148-156`):

```rust
    async fn snapshot(&self) -> anyhow::Result<WorkspaceSnapshot> {
        let paths = self.list("**").await?;
        let mut files = Vec::with_capacity(paths.len());
        for path in paths {
            // The floor, applied at the seam every caller shares. `list`'s dotfile skip is NOT
            // equivalent: the markers match as substrings, so `id_rsa` and `production.env`
            // pass it. A snapshot is the one `Workspace` operation that reads whole file
            // *contents* for shipment off-machine — `otto_remote::promote` puts one straight
            // into a `PromoteBundle` — so the floor is re-asserted here rather than in any one
            // caller, which would leave the next caller to reintroduce the gap.
            if otto_protocol::is_sensitive(&path.to_string_lossy()) {
                continue;
            }
            let bytes = self.read(&path).await?;
            files.push((path, bytes));
        }
        Ok(WorkspaceSnapshot { files })
    }
```

- [ ] **Step 4: Correct the false comment on the walk**

At `crates/workspace/src/lib.rs:84-87` the walk's comment reads "…and any dotfile/dotdir, which also covers the gate's sensitive-path floor". Replace that parenthetical:

```rust
        // Recursive mode (`**`): walk the subtree, returning files only. Skips a fixed set of
        // ignored directories (build/VCS/dependency dirs and any dotfile/dotdir). NOTE: the
        // dotfile skip is NOT the sensitive-path floor and must not be mistaken for it — the
        // floor's markers match as substrings, so `id_rsa` and `production.env` have no leading
        // dot and are returned by this walk. `snapshot` applies the floor itself for exactly
        // that reason. Does not follow symlinks, and caps the number of files to bound cost.
        // Output is sorted for determinism.
```

- [ ] **Step 5: Run the workspace tests**

Run: `cargo test -p otto-workspace`
Expected: PASS, including the new test and the pre-existing `snapshot_captures_listed_files_and_excludes_ignored`.

- [ ] **Step 6: Pin the guarantee where the bytes actually leave — a `remote` test**

Add to `crates/remote/src/lib.rs`'s `#[cfg(test)] mod tests`. Follow that module's existing fixtures for building a store/workspace and a fake `RemoteTarget`; if a fake target already exists, reuse it rather than adding another.

The test must: create a session, write `id_rsa` and `ok.txt` into the workspace, call `promote(...)`, capture the `PromoteBundle` the target received, and assert its `workspace.files` contains `ok.txt` and no path for which `otto_protocol::is_sensitive` is true.

This is the test that matters most — it pins the property at the layer that ships bytes off-machine, so a future refactor of `promote()` cannot quietly reintroduce the leak.

- [ ] **Step 7: Verify BOTH tests are load-bearing, by mutation**

Temporarily delete the `if otto_protocol::is_sensitive(...) { continue; }` guard from Step 3.
Run: `cargo test -p otto-workspace -p otto-remote`
Expected: **both** new tests FAIL. Restore the guard and confirm both pass again. Report the observed failure messages — do not report this step as done without them.

- [ ] **Step 8: Update `CLAUDE.md`**

In the crate table's `workspace` row, state that `snapshot()` applies the sensitive-path floor (the walk's dotfile skip is not equivalent), so a bundle built from it never carries a secret. Keep the table's existing one-cell style.

- [ ] **Step 9: Run the full gate**

```bash
cargo fmt --all
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace -- --skip rust_analyzer_integration
```
Expected: fmt clean, **zero** clippy warnings, all tests pass.

- [ ] **Step 10: Commit**

```bash
git add crates/workspace/src/lib.rs crates/remote/src/lib.rs CLAUDE.md
git commit -m "workspace: apply the sensitive-path floor in snapshot"
```

---

## Self-Review

**Spec coverage** *(updated post-implementation).* `RemoteWorkspace::snapshot` and the
`strip_sensitive_files` helper are not in the steps below — they came out of review and are
recorded in the Task Order note above and in the spec's rejected-alternatives section. Everything
else: Design §"Where the filter goes" → Step 3. The false-comment correction → Step 4. Testing §bullet 1 → Step 1. Testing §bullet 2 → Step 6. Testing §"verify by mutation" → Step 7. Success criteria 1 → Steps 1/5; 2 → Step 9; 3 (`promote()` unchanged) → enforced by the Global Constraints and by Step 6 testing it from the outside; 4 → Step 4.

**No placeholders.** Every step names an exact file, command, and expected result. Step 6 is the one that describes rather than dictates its code, because it must match `crates/remote`'s existing test fixtures, which the implementer should read rather than have guessed at here.

**Type consistency.** `otto_protocol::is_sensitive(&str) -> bool` is used identically in the impl and both tests. No signature changes anywhere.
