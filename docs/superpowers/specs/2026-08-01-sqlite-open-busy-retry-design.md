# `SqliteStore::open` must survive a concurrent first-open

> **Status:** IMPLEMENTED — fixes a flaky merge gate and the two real races behind it.
> **Found by:** `concurrent_opens_of_a_fresh_database_all_succeed` failing CI on an unrelated
> docs-only PR ([#130](https://github.com/robhicks/otto/pull/130)), and predicted by the quality
> review on [#128](https://github.com/robhicks/otto/pull/128).

Two otto processes opening the same fresh database concurrently can both fail with
`SQLITE_BUSY`. `otto serve &` followed by `otto run` in one directory does exactly this — both
resolve `otto-sessions.db` in the cwd — so this is a plausible first-run sequence, not a
contrived race.

---

## Provenance, stated plainly

The test that exposes this shipped in #123, as the regression guard for a schema-creation race.
The *bug* it exposes is older: `journal_mode(WAL)` has been in `open` since the crate was written.
So #123 added a correct test for a pre-existing defect and merged it without the fix, which made
the merge gate flaky for everyone. That is the actual mistake, and it is worth naming: a test that
reliably fails is a found bug, not a flaky test to re-run.

## The bug, measured

Two distinct steps can report `SQLITE_BUSY`, and neither is covered by what was there:

1. **`connect_with`.** Applying `journal_mode = WAL` takes an exclusive lock that `busy_timeout`
   **cannot wait on**. sqlx installs `sqlite3_busy_timeout` *before* the WAL pragma, so the timeout
   is set — it simply does not apply to this transition, as sqlx's own `options/mod.rs` states:
   *"changing into or out of it requires an exclusive lock that can't be waited on with
   `sqlite3_busy_timeout()`"* (launchbadge/sqlx#1930). Measured: of 8 concurrent connects to one
   fresh path, **6 failed at connect**.

   *(An earlier draft said the timeout "is not yet installed". That was the wrong mechanism, and
   it would have led a reader to try reordering connection setup. Corrected after review checked
   it against sqlx's source rather than against this document.)*
2. **`init_schema`.** Its `BEGIN IMMEDIATE` takes the write lock to create the schema. A
   connect-only retry still left roughly **30%** of opens failing here under CPU contention.

Controlled A/B, 20 runs each under identical load (4 competing CPU-bound processes):

| | failures | `database is locked` | `predates session ownership` |
|---|---|---|---|
| before | **15 / 20** | 14 | 1 |
| after the busy fix alone | **6 / 20** | 0 | 6 |
| after both fixes | **0 / 20** | 0 | 0 |

**There were two races, not one**, and fixing the first unmasked the second — found by review, not by
me. The residual was worse than the bug fixed: it told operators their brand-new database "predates
session ownership… delete the file".

## The second race: an un-transacted probe

`init_schema`'s read-only fast path ran `PRAGMA user_version` and the `sqlite_master` lookup as
**two statements in two implicit transactions**. In WAL each takes its own snapshot, so a concurrent
creator committing between them reads as `user_version == 0` (pre-commit) *with* `sessions` present
(post-commit) — the pre-ownership signature, on a perfectly good new database.

That fast path is mine, added in #128 to avoid taking a write lock on every open. It reintroduced
precisely the window `BEGIN IMMEDIATE` was written to close, and the surrounding comment claimed the
window was already shut. Fixed by probing inside a deferred `BEGIN` — one snapshot, still no write
lock, so the fast path's whole reason for existing is preserved.

## Fix

Retry the **whole** open under one budget, rather than either step individually — any step may
report busy, and the caller does not care which did.

```rust
// The timeout wraps the LOOP. A deadline checked only between attempts bounds nothing: one
// attempt can itself block for busy_timeout inside init_schema. Measured 19.65s against a
// nominal 10s before this was wrapped.
tokio::time::timeout(BUSY_BUDGET, async {
    loop {
        match Self::try_open(path).await {
            Ok(store) => return Ok(store),
            Err(e) if busy_somewhere(&e) => sleep(15ms + jitter).await,
            Err(e) => return Err(e),
        }
    }
}).await
```

A failed attempt also `close()`s its pool rather than dropping it: `PoolInner::drop` only *marks*
the pool closed, so the real `sqlite3_close` happens later on a worker thread — leaving handles
open against the very file we are waiting on, amplifying the contention being retried.

Two details that are load-bearing:

- **`is_busy` matches the primary result code** (`code & 0xff == 5`), so extended codes
  (`SQLITE_BUSY_SNAPSHOT` = 517, `SQLITE_BUSY_RECOVERY` = 261) count too. They all mean "another
  connection holds a lock; try again."
- **`busy_somewhere` downcasts through the `anyhow` chain.** `anyhow::Error::downcast_ref` already
  searches sources, so `.context()` layers do not hide the `sqlx::Error` — pinned by a test that
  goes red if a refactor replaces a `?` with `anyhow!("{e}")` and erases the concrete type.

  *(An earlier draft of this spec claimed a top-level downcast "compiled, looked right, and did not
  work". That was wrong: the mutation proving it fails does not fail, because anyhow searches the
  chain. The connect-only attempt failed for the single reason above it — `init_schema` is a second
  busy source — and misattributing it to the downcast was my error.)*

`busy_timeout` is *kept* on the connection options — it still covers statements on an established
connection, which is the common case and cheaper than retrying an open.

## Scope

**In:** the retry, the two predicates, and a `tokio` `time`-feature dependency on
`crates/persistence`.

**Out:** the schema-version guard, the ownership scoping, and anything outside `open`. The
existing `BEGIN IMMEDIATE` transaction stays exactly as it is — it fixes a *correctness* race
(a half-created schema being mistaken for a pre-ownership one) that this retry does not address.

### On the new dependency

`crates/persistence` gains `tokio = { features = ["time"] }`. This adds **no crate to the lock**:
sqlx's `runtime-tokio` feature already puts tokio in the graph, so this declares a dependency that
was already being built. Verified — `Cargo.lock` is unchanged.

The alternative, `std::thread::sleep`, would block an executor thread inside an async fn, which is
worse than a manifest line.

---

## Success Criteria

1. 20 consecutive runs of `concurrent_opens_of_a_fresh_database_all_succeed` under CPU load pass,
   where the same measurement before the fix failed 15 times.
2. `cargo test --workspace -- --skip rust_analyzer_integration` green; clippy clean under
   `-D warnings`; fmt clean.
3. `Cargo.lock` unchanged — no new crate enters the build.
4. A genuinely wedged database still fails rather than hanging: the budget is bounded.

## Risks

1. **A 10-second budget on a wedged file** delays a hard failure by up to 10s. Acceptable: the
   alternative is failing a legitimate concurrent start, and the budget is bounded rather than
   unbounded.
2. **Retrying hides contention** rather than removing it. True, and appropriate here — sqlite's
   own model is that writers serialize and callers retry; that is what `busy_timeout` exists to
   do for statements. This extends the same policy to the one step it cannot cover.
3. **Jitter is derived from the wall clock**, not a PRNG, to avoid a `rand` dependency in a leaf
   crate. Adequate for de-synchronising a handful of processes; not a substitute for real backoff
   if many engines ever share one file, which the per-owner-root work would change.
4. **`open_db_path()` resolves a *relative* `otto-sessions.db`**, so the working directory decides
   which processes share a database at all. `retrieval` already solved the same question properly
   — an OS cache dir keyed by workspace root — and `persistence` has not. Out of scope here, worth
   its own issue.
