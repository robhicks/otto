# Session Snapshot/Restore (Plan C) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add `snapshot`/`restore` to the `SessionStore`: capture a session's full state (metadata, config, complete event log, turn history) as a serializable `SessionState`, and write it back into a fresh store preserving the id and seqs — the promote-to-remote primitive.

**Architecture:** `SessionState` is a serde-serializable struct **derived from the existing tables** (no new table). `snapshot(session)` reads the `sessions` row + the full event log (via `replay_since(None)`) + the `turns` rows. `restore(&state)` inserts the `sessions` row preserving the original id/status/config, then replays events through `append_event` (preserving seqs) and turns through `record_turn`. Restore targets a fresh store (a remote engine's DB); restoring over an existing session collides on the `sessions` primary key and errors. This is Plan C of `docs/superpowers/specs/2026-06-16-persistence-distribution-axis-design.md`.

**Tech Stack:** Rust (edition 2024), `otto-persistence` (`SessionStore`/`SqliteStore`), `serde`/`serde_json`, `otto-protocol` (`Event`/`SessionId`), tokio/tempfile for tests.

**Scope notes / deliberate deferrals:**
- **`workspace_root` is deferred to `RemoteWorkspace`.** The spec listed a workspace-root reference in the snapshot, but the root is not available at the persistence boundary (`run_goal` takes `&dyn Workspace`, not a path; the `Workspace` trait does not expose its root) and a local path is unusable on another machine until the workspace-transfer mechanism (`RemoteWorkspace`, v2-deferred) exists. Capturing it now would mean wide signature churn for a field nothing can consume. `SessionState` snapshots id, goal, status, config, the full event log, and turn history.
- **Timestamps are not part of `SessionState`.** `created_at`/`updated_at`/`started_at` are storage metadata, not behavior; `restore` regenerates them. Excluding them keeps `SessionState` minimal and makes snapshot→restore→snapshot byte-identical at the `SessionState` level.
- This plan also lands the carried Plan-B review note: `session_config()` records the **effective** model (the `build_router` default when the env var is unset) instead of `null`, so a restored session's config reflects the routing it actually used.

---

### Task 1: `SessionState` type + serde derives

**Files:**
- Modify: `crates/persistence/src/types.rs`

- [ ] **Step 1: Write the failing serde round-trip test**

In `crates/persistence/src/types.rs`, add this test to the `#[cfg(test)] mod tests` block (after `session_status_rejects_unknown`):

```rust
    #[test]
    fn session_state_round_trips_through_json() {
        use otto_protocol::EventKind;
        let id = SessionId::new();
        let state = SessionState {
            id,
            goal: "the goal".to_string(),
            status: SessionStatus::Done,
            config: serde_json::json!({ "ollama": false }),
            events: vec![Event {
                seq: 0,
                session: id,
                kind: EventKind::Log {
                    message: "hi".to_string(),
                },
            }],
            turns: vec![TurnRecord {
                turn_index: 0,
                goal: "the goal".to_string(),
                outcome: serde_json::json!({ "ok": true }),
            }],
        };
        let json = serde_json::to_string(&state).unwrap();
        let back: SessionState = serde_json::from_str(&json).unwrap();
        assert_eq!(state, back);
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p otto-persistence types::tests::session_state_round_trips_through_json`
Expected: FAIL to compile — `SessionState` does not exist; `SessionStatus`/`TurnRecord` don't implement `Serialize`/`Deserialize`.

- [ ] **Step 3: Add the derives and the `SessionState` type**

In `crates/persistence/src/types.rs`:

Add imports at the very top of the file (below the module doc comment):

```rust
use otto_protocol::{Event, SessionId};
use serde::{Deserialize, Serialize};
```

Change the `SessionStatus` derive line from:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
```

to:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
```

Change the `TurnRecord` derive line from:

```rust
#[derive(Debug, Clone, PartialEq)]
```

to:

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
```

Add the `SessionState` struct after the `TurnRecord` definition (before the `#[cfg(test)]` block):

```rust
/// The full, serializable state of a session — its metadata, config, complete event log,
/// and turn history — derived from the store's tables. Used to move a session between
/// engines (snapshot on one, restore on another). The workspace patch-bundle is deferred
/// until `RemoteWorkspace`; timestamps are storage metadata and are not captured.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionState {
    pub id: SessionId,
    pub goal: String,
    pub status: SessionStatus,
    pub config: serde_json::Value,
    pub events: Vec<Event>,
    pub turns: Vec<TurnRecord>,
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p otto-persistence types::`
Expected: PASS (3 tests).

- [ ] **Step 5: Commit**

```bash
git add crates/persistence/src/types.rs
git commit -m "feat(persistence): SessionState type + serde derives"
```

---

### Task 2: `snapshot`

**Files:**
- Modify: `crates/persistence/src/lib.rs`
- Modify: `crates/persistence/src/sqlite.rs`

- [ ] **Step 1: Add `snapshot` to the trait and export `SessionState`**

In `crates/persistence/src/lib.rs`:

Change the re-export line from:

```rust
pub use types::{SessionStatus, TurnRecord};
```

to:

```rust
pub use types::{SessionState, SessionStatus, TurnRecord};
```

Add the `snapshot` method to the `SessionStore` trait (after `session_status`):

```rust
    /// Capture the full state of `session` — metadata, config, the complete event log, and
    /// turn history — as a serializable `SessionState`. Errors if the session does not
    /// exist. (The workspace patch-bundle is deferred until `RemoteWorkspace`.)
    async fn snapshot(&self, session: SessionId) -> anyhow::Result<SessionState>;
```

- [ ] **Step 2: Write the failing tests**

In `crates/persistence/src/sqlite.rs`, add to the `#[cfg(test)] mod tests` block (the `log_event`, `turn`, and `temp_store` helpers already exist there):

```rust
    #[tokio::test]
    async fn snapshot_captures_metadata_events_and_turns() {
        let (store, _dir) = temp_store().await;
        let id = store
            .create_session("the goal", &serde_json::json!({ "k": 1 }))
            .await
            .unwrap();
        store.append_event(id, &log_event(id, 0, "a")).await.unwrap();
        store.append_event(id, &log_event(id, 1, "b")).await.unwrap();
        store.record_turn(id, &turn(0, true)).await.unwrap();
        store.set_status(id, SessionStatus::Done).await.unwrap();

        let snap = store.snapshot(id).await.unwrap();
        assert_eq!(snap.id, id);
        assert_eq!(snap.goal, "the goal");
        assert_eq!(snap.status, SessionStatus::Done);
        assert_eq!(snap.config, serde_json::json!({ "k": 1 }));
        assert_eq!(snap.events, vec![log_event(id, 0, "a"), log_event(id, 1, "b")]);
        assert_eq!(snap.turns, vec![turn(0, true)]);
    }

    #[tokio::test]
    async fn snapshot_unknown_session_is_error() {
        let (store, _dir) = temp_store().await;
        let missing = otto_protocol::SessionId::new();
        assert!(store.snapshot(missing).await.is_err());
    }
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p otto-persistence sqlite::tests::snapshot`
Expected: FAIL to compile — `snapshot` is not implemented for `SqliteStore` (the trait now requires it).

- [ ] **Step 4: Implement `snapshot`**

In `crates/persistence/src/sqlite.rs`, add this method to the `impl crate::SessionStore for SqliteStore` block (after `session_status`):

```rust
    async fn snapshot(
        &self,
        session: otto_protocol::SessionId,
    ) -> anyhow::Result<crate::SessionState> {
        let row: Option<(String, String, String)> =
            sqlx::query_as("SELECT goal, status, config FROM sessions WHERE id = ?1")
                .bind(session.0.to_string())
                .fetch_optional(&self.pool)
                .await?;
        let (goal, status, config) =
            row.ok_or_else(|| anyhow::anyhow!("snapshot: no session {}", session.0))?;
        let status = crate::SessionStatus::from_db_str(&status)?;
        let config: serde_json::Value = serde_json::from_str(&config)?;

        let events = self.replay_since(session, None).await?;

        let turn_rows: Vec<(i64, String, String)> = sqlx::query_as(
            "SELECT turn_index, goal, outcome FROM turns
             WHERE session_id = ?1 ORDER BY turn_index ASC",
        )
        .bind(session.0.to_string())
        .fetch_all(&self.pool)
        .await?;
        let mut turns = Vec::with_capacity(turn_rows.len());
        for (turn_index, turn_goal, outcome) in turn_rows {
            turns.push(crate::TurnRecord {
                turn_index: turn_index as u32,
                goal: turn_goal,
                outcome: serde_json::from_str(&outcome)?,
            });
        }

        Ok(crate::SessionState {
            id: session,
            goal,
            status,
            config,
            events,
            turns,
        })
    }
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p otto-persistence sqlite::tests::snapshot`
Expected: PASS (2 tests).

- [ ] **Step 6: Commit**

```bash
git add crates/persistence/src/lib.rs crates/persistence/src/sqlite.rs
git commit -m "feat(persistence): snapshot a session into SessionState"
```

---

### Task 3: `restore`

**Files:**
- Modify: `crates/persistence/src/lib.rs`
- Modify: `crates/persistence/src/sqlite.rs`

- [ ] **Step 1: Add `restore` to the trait**

In `crates/persistence/src/lib.rs`, add the `restore` method to the `SessionStore` trait (after `snapshot`):

```rust
    /// Write a previously captured `SessionState` into this store, preserving its id, seqs,
    /// status, config, and turn history. Intended for a fresh store (e.g. a remote engine);
    /// errors if the session id already exists. Returns the (preserved) session id.
    async fn restore(&self, state: &SessionState) -> anyhow::Result<SessionId>;
```

- [ ] **Step 2: Write the failing tests**

In `crates/persistence/src/sqlite.rs`, add to `mod tests`:

```rust
    #[tokio::test]
    async fn snapshot_restore_round_trips_into_a_fresh_store() {
        let (source, _d1) = temp_store().await;
        let id = source
            .create_session("g", &serde_json::json!({ "m": "x" }))
            .await
            .unwrap();
        source.append_event(id, &log_event(id, 0, "a")).await.unwrap();
        source.append_event(id, &log_event(id, 1, "b")).await.unwrap();
        source.record_turn(id, &turn(0, true)).await.unwrap();
        source.set_status(id, SessionStatus::Done).await.unwrap();
        let snap = source.snapshot(id).await.unwrap();

        let (target, _d2) = temp_store().await;
        let restored_id = target.restore(&snap).await.unwrap();
        assert_eq!(restored_id, id);

        // Re-snapshotting the target yields an identical SessionState (preserved id/seqs).
        assert_eq!(target.snapshot(id).await.unwrap(), snap);
        assert_eq!(target.replay_since(id, None).await.unwrap(), snap.events);
        assert_eq!(target.session_status(id).await.unwrap(), SessionStatus::Done);
    }

    #[tokio::test]
    async fn restore_into_existing_session_is_error() {
        let (store, _dir) = temp_store().await;
        let id = store
            .create_session("g", &serde_json::json!({}))
            .await
            .unwrap();
        let snap = store.snapshot(id).await.unwrap();
        // Restoring into the same store collides on the sessions primary key.
        assert!(store.restore(&snap).await.is_err());
    }
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p otto-persistence sqlite::tests::restore_into_existing_session_is_error`
Expected: FAIL to compile — `restore` is not implemented for `SqliteStore`.

- [ ] **Step 4: Implement `restore`**

In `crates/persistence/src/sqlite.rs`, add this method to the `impl crate::SessionStore for SqliteStore` block (after `snapshot`):

```rust
    async fn restore(
        &self,
        state: &crate::SessionState,
    ) -> anyhow::Result<otto_protocol::SessionId> {
        // Insert the session row preserving id/status/config/goal (timestamps regenerated).
        let now = now_millis();
        sqlx::query(
            "INSERT INTO sessions (id, goal, status, created_at, updated_at, config)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )
        .bind(state.id.0.to_string())
        .bind(&state.goal)
        .bind(state.status.as_db_str())
        .bind(now)
        .bind(now)
        .bind(serde_json::to_string(&state.config)?)
        .execute(&self.pool)
        .await?;

        // Replay events (preserving seqs) and turns through the existing inserts.
        for event in &state.events {
            self.append_event(state.id, event).await?;
        }
        for turn in &state.turns {
            self.record_turn(state.id, turn).await?;
        }

        Ok(state.id)
    }
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p otto-persistence sqlite::tests::`
Expected: PASS (all sqlite tests, including the two new restore tests and the round-trip).

- [ ] **Step 6: Commit**

```bash
git add crates/persistence/src/lib.rs crates/persistence/src/sqlite.rs
git commit -m "feat(persistence): restore a SessionState into a fresh store"
```

---

### Task 4: `session_config()` records the effective model (Plan-B carry-over)

**Files:**
- Modify: `crates/engine/src/lib.rs`

- [ ] **Step 1: Record the effective model defaults instead of null**

In `crates/engine/src/lib.rs`, in the `session_config()` function, replace these two lines:

```rust
        "ollama_model": std::env::var("OTTO_OLLAMA_MODEL").ok(),
        "anthropic_model": std::env::var("OTTO_ANTHROPIC_MODEL").ok(),
```

with:

```rust
        // Record the EFFECTIVE model (the build_router default when the env var is unset),
        // so a restored session's config reflects the routing it actually used.
        "ollama_model": std::env::var("OTTO_OLLAMA_MODEL")
            .unwrap_or_else(|_| "llama3.2".to_string()),
        "anthropic_model": std::env::var("OTTO_ANTHROPIC_MODEL")
            .unwrap_or_else(|_| "claude-haiku-4-5".to_string()),
```

No new test: `session_config()` reads process-global env, and the engine lib test binary already manipulates `OTTO_*`/`ANTHROPIC_API_KEY` under a documented single-test no-race assumption; adding an env-reading test here would race. The defaults mirror `build_router`'s own (`llama3.2`, `claude-haiku-4-5`) — keep them in sync if those ever change.

- [ ] **Step 2: Verify the engine still builds and tests pass**

Run: `cargo test -p otto-engine`
Expected: PASS (unchanged test count; the edit only changes the JSON `session_config` produces).

- [ ] **Step 3: Commit**

```bash
git add crates/engine/src/lib.rs
git commit -m "fix(engine): session_config records the effective model, not null"
```

---

### Task 5: Full workspace verification

**Files:** none (verification only)

- [ ] **Step 1: Format, lint, and test the whole workspace**

Run: `cargo fmt --all && cargo clippy --workspace --all-targets && cargo test --workspace`
Expected: fmt clean (or trivial changes you then include), clippy clean across the workspace, and all tests pass — the new persistence snapshot/restore tests plus every existing test unchanged.

- [ ] **Step 2: If `cargo fmt` changed anything, commit it**

```bash
git add -A
git commit -m "style: cargo fmt after snapshot/restore"
```

(If fmt made no changes, skip this commit.)

---

## Done criteria

- `SessionState` (serde-serializable) capturing id, goal, status, config, event log, and turn history; round-trips through JSON.
- `SessionStore::snapshot(session)` returns the full `SessionState` (errors on unknown session); `restore(&state)` writes it into a fresh store preserving id/seqs/status/config/turns and returns the id (errors if the session already exists).
- snapshot→restore→snapshot is identical; the restored event log replays identically.
- `session_config()` records the effective model rather than `null`.
- `cargo test --workspace` green; clippy/fmt clean.

**Arc complete after this plan.** The persistence/distribution-axis foundation (store → lifecycle → snapshot) is in place. Remaining distribution-axis work lives in future arcs: a `serve` transport exposing `replay_since` over the wire for `Last-Event-ID` reconnect, and `RemoteWorkspace` (which is where the deferred `workspace_root` + patch-bundle land).
