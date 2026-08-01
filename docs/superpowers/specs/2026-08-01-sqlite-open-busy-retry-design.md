# `SqliteStore::open` must survive a concurrent first-open

> **Status:** DRAFT — fixes a flaky merge gate and the real robustness bug behind it.
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

1. **`connect_with`.** Applying `journal_mode = WAL` takes a brief exclusive lock. sqlite's own
   `busy_timeout` is a per-connection setting that is not yet installed when this runs, so the
   busy handler never fires. Measured directly: of 8 concurrent connects to one fresh path, **6
   failed at connect**.
2. **`init_schema`.** Its `BEGIN IMMEDIATE` takes the write lock to create the schema. A
   connect-only retry still left roughly **30%** of opens failing here under CPU contention.

Controlled A/B, 20 runs each under identical load (4 competing CPU-bound processes):

| | failures |
|---|---|
| before | **15 / 20** |
| after | **0 / 20** |

## Fix

Retry the **whole** open under one budget, rather than either step individually — any step may
report busy, and the caller does not care which did.

```rust
let deadline = Instant::now() + BUSY_BUDGET;   // 10s, aggregate
loop {
    match Self::try_open(path).await {
        Ok(store) => return Ok(store),
        Err(e) if Instant::now() < deadline && busy_somewhere(&e) => sleep(20ms).await,
        Err(e) => return Err(e),
    }
}
```

Two details that are load-bearing:

- **`is_busy` matches the primary result code** (`code & 0xff == 5`), so extended codes
  (`SQLITE_BUSY_SNAPSHOT` = 517, `SQLITE_BUSY_RECOVERY` = 261) count too. They all mean "another
  connection holds a lock; try again."
- **`busy_somewhere` walks the `anyhow` chain.** `open` returns `anyhow::Error`, which erases the
  `sqlx::Error`, so a top-level downcast finds nothing. This is why an earlier connect-only
  version appeared to work and did not.

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
3. **The 20 ms delay is not jittered**, so N racing processes retry in lockstep. Harmless at the
   concurrency otto actually sees (a handful of processes); worth revisiting only if a real
   deployment ever runs many engines against one file, which the per-owner-root work would change.
