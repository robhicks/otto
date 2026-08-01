# SQLite Open Busy-Retry Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

> **Status:** SHIPPED 2026-08-01 — one task, landed as a single commit. Written after the fact
> against the measured before/after, since the fix was diagnosed and verified in one pass while
> the merge gate was red.

**Goal:** Make `SqliteStore::open` survive a concurrent first-open of the same fresh database, fixing both the flaky merge-gate test and the real robustness bug it was reporting.

**Architecture:** One bounded retry around the *whole* open, because either the connect (`journal_mode = WAL` takes an exclusive lock before `busy_timeout` is installed) or `init_schema` (`BEGIN IMMEDIATE`) can report `SQLITE_BUSY`, and the caller does not care which did.

**Tech Stack:** Rust (edition 2024, toolchain 1.97.0), `sqlx` 0.8 sqlite, `tokio` (`time` feature only — already in the graph via sqlx's `runtime-tokio`, so no new crate).

**Spec:** `docs/superpowers/specs/2026-08-01-sqlite-open-busy-retry-design.md` — read it first, in particular the measured failure split between the two steps.

## Global Constraints

- **No new crate in `Cargo.lock`.** The `tokio` dependency is a manifest declaration of something sqlx already builds. Verify with `git diff --stat Cargo.lock` — it must be empty.
- **Do not touch the `BEGIN IMMEDIATE` transaction, the `PRAGMA user_version` guard, or the ownership scoping.** They solve a different problem (a half-created schema being mistaken for a pre-ownership one) and this change does not subsume them.
- **Keep `busy_timeout` on the connection options.** It still covers statements on an established connection — the common case, and cheaper than retrying a whole open.
- Determinism holds: no env read, no network. `ui-dioxus/` untouched.
- No AI attribution in any commit message, comment, or doc.
- Gate: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace -- --skip rust_analyzer_integration`.

## File Structure

| File | Responsibility |
|---|---|
| `crates/persistence/src/sqlite.rs` | **Modify.** `open` retries; `try_open` holds one attempt; `is_busy` + `busy_somewhere` classify. |
| `crates/persistence/Cargo.toml` | **Modify.** `tokio` with the `time` feature, with a comment recording that it adds nothing to the lock. |

## Task Order & Rationale

One task. The diagnosis, the fix, and the measurement are inseparable here — the first two attempts (connect-only retry; then a whole-open retry with a top-level downcast) both *looked* right and were disproved only by re-measuring, so the verification step is the substance of the task rather than a formality.

---

### Task 1: Retry the whole open on `SQLITE_BUSY`

**Files:**
- Modify: `crates/persistence/src/sqlite.rs`
- Modify: `crates/persistence/Cargo.toml`

- [x] **Step 1: Establish the baseline, under controlled load**

Kill any stray load processes first, pre-build so compilation never lands inside a timed run, then start a fixed number of CPU-bound competitors and run the existing test 20 times:

```bash
pkill yes; cargo test -p otto-persistence concurrent_opens >/dev/null 2>&1
for i in 1 2 3 4; do (yes > /dev/null &); done
# 20 runs, counting `^test result: FAILED`
```

Observed: **15 / 20 failed.** Record this — it is the number the fix has to move.

- [x] **Step 2: Locate which step actually reports busy**

Do not assume. A throwaway test that connects 8 times concurrently and reports per-task where the error arose showed **6 of 8 failing at `connect_with`**, not at `BEGIN IMMEDIATE`. That is why the fix cannot live inside `init_schema`.

- [x] **Step 3: Add the predicates**

```rust
const BUSY_BUDGET: std::time::Duration = std::time::Duration::from_secs(10);

/// Matched on the primary result code so extended codes (SQLITE_BUSY_SNAPSHOT = 517,
/// SQLITE_BUSY_RECOVERY = 261) count too — they all mean "try again".
fn is_busy(e: &sqlx::Error) -> bool {
    let sqlx::Error::Database(db) = e else { return false };
    db.code().and_then(|c| c.parse::<i32>().ok()).is_some_and(|c| c & 0xff == 5)
}

/// `open` returns `anyhow::Error`, which erases the `sqlx::Error` — a top-level downcast finds
/// nothing, so walk the chain.
fn busy_somewhere(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.downcast_ref::<sqlx::Error>().is_some_and(is_busy))
}
```

- [x] **Step 4: Split `open` into a retry loop plus `try_open`**

`try_open` is the previous body verbatim. `open` loops on it under `BUSY_BUDGET`, sleeping 20 ms between attempts, returning any non-busy error immediately.

- [x] **Step 5: Declare the `tokio` dependency**

`tokio = { workspace = true, features = ["time"] }` under `[dependencies]`, with a comment noting sqlx's `runtime-tokio` already puts it in the graph.

- [x] **Step 6: Re-measure, identically**

Same load, same 20 runs, and — critically — **kill stray load processes between measurements.** An earlier comparison was contaminated by competitors left running from a previous block and produced a number that disagreed with a clean re-run.

Observed: **0 / 20 failed**, against 15 / 20 before.

- [x] **Step 7: Confirm no new crate**

```bash
git diff --stat Cargo.lock   # must be empty
```

- [x] **Step 8: Full gate, then commit**

```bash
cargo fmt --all && cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace -- --skip rust_analyzer_integration
git commit -m "persistence: retry SqliteStore::open while sqlite reports the database is locked"
```

---

## Self-Review

**Spec coverage.** §"The bug, measured" → Steps 1–2. §Fix and both load-bearing details → Steps 3–4. §"On the new dependency" → Steps 5, 7. Success criteria 1 → Step 6; 2 → Step 8; 3 → Step 7; 4 → the bounded `BUSY_BUDGET` in Step 3.

**The one thing worth carrying forward:** two plausible fixes were disproved by measurement, not by review — a connect-only retry, and a whole-open retry whose predicate downcast at the top level and so never matched through `anyhow`. Both compiled, both looked correct, and both left the test failing. Re-measure after every attempt.
