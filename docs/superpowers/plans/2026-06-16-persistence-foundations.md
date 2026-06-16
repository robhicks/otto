# Persistence Foundations (Plan A) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Create a new `otto-persistence` crate with a `SessionStore` trait and a sqlx-backed `SqliteStore` that persists sessions, their event log (seq-ordered), and turn records, with gap-correct event replay.

**Architecture:** A leaf crate depending only on `otto-protocol`. It owns both the `SessionStore` trait and the `SqliteStore` impl (the engine will depend on it directly later). Schema is created idempotently with plain `CREATE TABLE IF NOT EXISTS` at open time — no migrations dir, no compile-time DB. This is the first of three plans from `docs/superpowers/specs/2026-06-16-persistence-distribution-axis-design.md`; lifecycle/engine-wiring (Plan B) and `SessionState` snapshot (Plan C) follow.

**Tech Stack:** Rust (edition 2024), `sqlx` 0.8 (runtime queries, sqlite, tokio runtime), `serde_json`, `async-trait`, `anyhow`, `tokio`/`tempfile` for tests.

**Scope note:** Plan A's `SessionStore` trait deliberately omits `snapshot()`/`restore()` and the `SessionState` type — those (and the timestamp/`config` consumers) arrive in Plan C. The `sessions.config` column is written now so the schema is stable, fed an empty JSON object until Plan B supplies real config.

---

### Task 1: Scaffold the `otto-persistence` crate

**Files:**
- Create: `crates/persistence/Cargo.toml`
- Create: `crates/persistence/src/lib.rs`
- Modify: `Cargo.toml` (workspace members)

- [ ] **Step 1: Create the crate manifest**

Create `crates/persistence/Cargo.toml`:

```toml
[package]
name = "otto-persistence"
version = "0.1.0"
edition.workspace = true
rust-version.workspace = true
license.workspace = true

[dependencies]
otto-protocol = { path = "../protocol" }
async-trait.workspace = true
anyhow.workspace = true
serde = { workspace = true }
serde_json = { workspace = true }
sqlx = { version = "0.8", default-features = false, features = ["runtime-tokio", "sqlite"] }

[dev-dependencies]
tempfile.workspace = true
tokio = { workspace = true, features = ["macros", "rt-multi-thread"] }
```

- [ ] **Step 2: Create a placeholder lib so the crate compiles**

Create `crates/persistence/src/lib.rs`:

```rust
//! Durable session store for the engine: persists sessions, their seq-ordered event
//! log, and turn records to sqlite, with gap-correct event replay. The `engine` layer
//! depends on this crate directly and holds a `Box<dyn SessionStore>`.

mod sqlite;

pub use sqlite::SqliteStore;
```

This will not compile yet (no `sqlite` module). That is fixed in Task 2; for now create the module file empty so the crate builds:

Create `crates/persistence/src/sqlite.rs`:

```rust
// SqliteStore lands in Task 3.
```

And temporarily make `lib.rs` not reference it yet — replace the `lib.rs` body with just the doc comment for this step:

```rust
//! Durable session store for the engine: persists sessions, their seq-ordered event
//! log, and turn records to sqlite, with gap-correct event replay. The `engine` layer
//! depends on this crate directly and holds a `Box<dyn SessionStore>`.
```

- [ ] **Step 3: Register the crate in the workspace**

In `Cargo.toml` (repo root), add `"crates/persistence",` to the `members` array. The result:

```toml
members = [
    "crates/protocol",
    "crates/engine-core",
    "crates/workspace",
    "crates/providers",
    "crates/router",
    "crates/tools",
    "crates/agents",
    "crates/engine",
    "crates/persistence",
]
```

- [ ] **Step 4: Verify it builds**

Run: `cargo build -p otto-persistence`
Expected: PASS (downloads/compiles sqlx, builds an empty lib).

- [ ] **Step 5: Commit**

```bash
git add crates/persistence/Cargo.toml crates/persistence/src/lib.rs crates/persistence/src/sqlite.rs Cargo.toml Cargo.lock
git commit -m "feat(persistence): scaffold otto-persistence crate"
```

---

### Task 2: Define core types and the `SessionStore` trait

**Files:**
- Create: `crates/persistence/src/types.rs`
- Modify: `crates/persistence/src/lib.rs`

- [ ] **Step 1: Write the failing test for `SessionStatus` round-tripping**

Create `crates/persistence/src/types.rs`:

```rust
//! Public types for the session store: session status and the turn record written
//! per orchestrator turn.

/// Lifecycle status of a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionStatus {
    Active,
    Done,
    Aborted,
    Failed,
}

impl SessionStatus {
    /// The string stored in the `sessions.status` column.
    pub fn as_db_str(&self) -> &'static str {
        match self {
            SessionStatus::Active => "active",
            SessionStatus::Done => "done",
            SessionStatus::Aborted => "aborted",
            SessionStatus::Failed => "failed",
        }
    }

    /// Parse a status back from its `sessions.status` column value.
    pub fn from_db_str(s: &str) -> anyhow::Result<Self> {
        Ok(match s {
            "active" => SessionStatus::Active,
            "done" => SessionStatus::Done,
            "aborted" => SessionStatus::Aborted,
            "failed" => SessionStatus::Failed,
            other => anyhow::bail!("unknown session status: {other:?}"),
        })
    }
}

/// One orchestrator turn's record. `outcome` is a JSON value so the store stays
/// decoupled from `engine-core`'s `TurnOutcome` (the engine layer serializes it).
#[derive(Debug, Clone, PartialEq)]
pub struct TurnRecord {
    pub turn_index: u32,
    pub goal: String,
    pub outcome: serde_json::Value,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_status_round_trips_through_db_str() {
        for s in [
            SessionStatus::Active,
            SessionStatus::Done,
            SessionStatus::Aborted,
            SessionStatus::Failed,
        ] {
            assert_eq!(SessionStatus::from_db_str(s.as_db_str()).unwrap(), s);
        }
    }

    #[test]
    fn session_status_rejects_unknown() {
        assert!(SessionStatus::from_db_str("bogus").is_err());
    }
}
```

- [ ] **Step 2: Add the `SessionStore` trait to `lib.rs`**

Replace `crates/persistence/src/lib.rs` with:

```rust
//! Durable session store for the engine: persists sessions, their seq-ordered event
//! log, and turn records to sqlite, with gap-correct event replay. The `engine` layer
//! depends on this crate directly and holds a `Box<dyn SessionStore>`.

mod sqlite;
mod types;

use async_trait::async_trait;
use otto_protocol::{Event, SessionId};
use serde_json::Value;

pub use sqlite::SqliteStore;
pub use types::{SessionStatus, TurnRecord};

/// Persists sessions and their event/turn history. Implementations are `Send + Sync`
/// so the engine can hold one as `Box<dyn SessionStore>` across await points.
#[async_trait]
pub trait SessionStore: Send + Sync {
    /// Create a new session for `goal`, storing `config` (router/model selection) as
    /// JSON. Returns the new session id.
    async fn create_session(&self, goal: &str, config: &Value) -> anyhow::Result<SessionId>;

    /// Append one already-sequenced event to the session's log. `(session, seq)` is a
    /// primary key, so re-appending the same seq is an error.
    async fn append_event(&self, session: SessionId, event: &Event) -> anyhow::Result<()>;

    /// Record one completed orchestrator turn.
    async fn record_turn(&self, session: SessionId, turn: &TurnRecord) -> anyhow::Result<()>;

    /// Update a session's lifecycle status. Errors if the session does not exist.
    async fn set_status(&self, session: SessionId, status: SessionStatus) -> anyhow::Result<()>;

    /// Replay the session's events with `seq > after_seq`, in ascending seq order.
    /// Pass `0` to get the full log (seqs are 0-based, so this returns everything).
    async fn replay_since(
        &self,
        session: SessionId,
        after_seq: u64,
    ) -> anyhow::Result<Vec<Event>>;
}
```

- [ ] **Step 3: Run the type tests to verify they pass**

Run: `cargo test -p otto-persistence types::`
Expected: PASS (2 tests). The crate compiles because `sqlite.rs` is still an empty module and the trait references only `otto-protocol` types.

- [ ] **Step 4: Commit**

```bash
git add crates/persistence/src/lib.rs crates/persistence/src/types.rs
git commit -m "feat(persistence): add SessionStatus, TurnRecord, and SessionStore trait"
```

---

### Task 3: `SqliteStore::open` + idempotent schema

**Files:**
- Modify: `crates/persistence/src/sqlite.rs`

- [ ] **Step 1: Write the failing test for opening a store**

Replace `crates/persistence/src/sqlite.rs` with:

```rust
//! Sqlite-backed `SessionStore`. Schema is created idempotently at open time with
//! `CREATE TABLE IF NOT EXISTS`, so no migrations dir or compile-time DB is needed and
//! the build/test path stays fully offline.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

/// A session store backed by a single sqlite database file.
pub struct SqliteStore {
    pool: SqlitePool,
}

impl SqliteStore {
    /// Open (creating if absent) the sqlite database at `path` and ensure the schema
    /// exists.
    pub async fn open(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let opts = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true);
        let pool = SqlitePoolOptions::new().connect_with(opts).await?;
        let store = Self { pool };
        store.init_schema().await?;
        Ok(store)
    }

    async fn init_schema(&self) -> anyhow::Result<()> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS sessions (
                id TEXT PRIMARY KEY,
                goal TEXT NOT NULL,
                status TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                config TEXT NOT NULL
            )",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS events (
                session_id TEXT NOT NULL,
                seq INTEGER NOT NULL,
                kind TEXT NOT NULL,
                PRIMARY KEY (session_id, seq)
            )",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS turns (
                session_id TEXT NOT NULL,
                turn_index INTEGER NOT NULL,
                goal TEXT NOT NULL,
                outcome TEXT NOT NULL,
                started_at INTEGER NOT NULL,
                PRIMARY KEY (session_id, turn_index)
            )",
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

/// Milliseconds since the Unix epoch, for `created_at`/`updated_at`/`started_at`.
fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Opens a fresh store in a temp dir. The returned `TempDir` must be kept alive for
    /// the duration of the test so the database file is not deleted.
    async fn temp_store() -> (SqliteStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteStore::open(dir.path().join("test.db")).await.unwrap();
        (store, dir)
    }

    #[tokio::test]
    async fn open_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        // Opening twice over the same file must not error (schema uses IF NOT EXISTS).
        let _first = SqliteStore::open(&path).await.unwrap();
        let _second = SqliteStore::open(&path).await.unwrap();
    }

    #[tokio::test]
    async fn now_millis_is_positive() {
        let (_store, _dir) = temp_store().await;
        assert!(now_millis() > 0);
    }
}
```

- [ ] **Step 2: Run the tests to verify they pass**

Run: `cargo test -p otto-persistence sqlite::`
Expected: PASS (2 tests). `now_millis` is currently used only by the test; the `#[allow(dead_code)]`-free warning is acceptable because Task 4 uses it. If a dead-code warning blocks a `-D warnings` build, it is resolved in Task 4.

- [ ] **Step 3: Commit**

```bash
git add crates/persistence/src/sqlite.rs
git commit -m "feat(persistence): SqliteStore::open with idempotent schema"
```

---

### Task 4: `create_session` + status read helper

**Files:**
- Modify: `crates/persistence/src/sqlite.rs`

- [ ] **Step 1: Write the failing test**

In `crates/persistence/src/sqlite.rs`, add this test inside the existing `mod tests` block (after `now_millis_is_positive`):

```rust
    #[tokio::test]
    async fn create_session_starts_active() {
        let (store, _dir) = temp_store().await;
        let id = store
            .create_session("add a greeting", &serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(store.session_status(id).await.unwrap(), SessionStatus::Active);
    }
```

Add these imports at the top of the test module (just under `use super::*;`):

```rust
    use crate::{SessionStatus, SessionStore};
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p otto-persistence sqlite::create_session_starts_active`
Expected: FAIL to compile — `create_session` and `session_status` are not defined on `SqliteStore`.

- [ ] **Step 3: Implement `create_session` (trait impl) and `session_status` (inherent helper)**

Add the `SessionStore` impl block and the inherent helper to `crates/persistence/src/sqlite.rs`, after the `impl SqliteStore { ... }` block and before `fn now_millis`:

```rust
impl SqliteStore {
    /// Read a session's current status. Errors if the session does not exist.
    pub async fn session_status(
        &self,
        session: otto_protocol::SessionId,
    ) -> anyhow::Result<crate::SessionStatus> {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT status FROM sessions WHERE id = ?1")
                .bind(session.0.to_string())
                .fetch_optional(&self.pool)
                .await?;
        let (status,) = row
            .ok_or_else(|| anyhow::anyhow!("session_status: no session {}", session.0))?;
        crate::SessionStatus::from_db_str(&status)
    }
}

#[async_trait::async_trait]
impl crate::SessionStore for SqliteStore {
    async fn create_session(
        &self,
        goal: &str,
        config: &serde_json::Value,
    ) -> anyhow::Result<otto_protocol::SessionId> {
        let id = otto_protocol::SessionId::new();
        let now = now_millis();
        sqlx::query(
            "INSERT INTO sessions (id, goal, status, created_at, updated_at, config)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )
        .bind(id.0.to_string())
        .bind(goal)
        .bind(crate::SessionStatus::Active.as_db_str())
        .bind(now)
        .bind(now)
        .bind(serde_json::to_string(config)?)
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    async fn append_event(
        &self,
        _session: otto_protocol::SessionId,
        _event: &otto_protocol::Event,
    ) -> anyhow::Result<()> {
        unimplemented!("append_event lands in Task 5")
    }

    async fn record_turn(
        &self,
        _session: otto_protocol::SessionId,
        _turn: &crate::TurnRecord,
    ) -> anyhow::Result<()> {
        unimplemented!("record_turn lands in Task 7")
    }

    async fn set_status(
        &self,
        _session: otto_protocol::SessionId,
        _status: crate::SessionStatus,
    ) -> anyhow::Result<()> {
        unimplemented!("set_status lands in Task 6")
    }

    async fn replay_since(
        &self,
        _session: otto_protocol::SessionId,
        _after_seq: u64,
    ) -> anyhow::Result<Vec<otto_protocol::Event>> {
        unimplemented!("replay_since lands in Task 5")
    }
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p otto-persistence sqlite::create_session_starts_active`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/persistence/src/sqlite.rs
git commit -m "feat(persistence): create_session + session_status read"
```

---

### Task 5: `append_event` + `replay_since`

**Files:**
- Modify: `crates/persistence/src/sqlite.rs`

- [ ] **Step 1: Write the failing tests**

Add these tests inside `mod tests`, and extend the test imports to include `Event`/`EventKind`:

Update the test imports line to:

```rust
    use crate::{SessionStatus, SessionStore};
    use otto_protocol::{Event, EventKind};
```

Add the tests:

```rust
    fn log_event(session: otto_protocol::SessionId, seq: u64, msg: &str) -> Event {
        Event {
            seq,
            session,
            kind: EventKind::Log { message: msg.to_string() },
        }
    }

    #[tokio::test]
    async fn append_then_replay_returns_events_in_order() {
        let (store, _dir) = temp_store().await;
        let id = store.create_session("g", &serde_json::json!({})).await.unwrap();
        for (seq, msg) in [(0u64, "a"), (1, "b"), (2, "c")] {
            store.append_event(id, &log_event(id, seq, msg)).await.unwrap();
        }
        let replayed = store.replay_since(id, 0).await.unwrap();
        assert_eq!(replayed, vec![
            log_event(id, 0, "a"),
            log_event(id, 1, "b"),
            log_event(id, 2, "c"),
        ]);
    }

    #[tokio::test]
    async fn replay_since_returns_only_the_gap() {
        let (store, _dir) = temp_store().await;
        let id = store.create_session("g", &serde_json::json!({})).await.unwrap();
        for (seq, msg) in [(0u64, "a"), (1, "b"), (2, "c")] {
            store.append_event(id, &log_event(id, seq, msg)).await.unwrap();
        }
        let gap = store.replay_since(id, 1).await.unwrap();
        assert_eq!(gap, vec![log_event(id, 2, "c")]);
    }

    #[tokio::test]
    async fn append_duplicate_seq_is_error() {
        let (store, _dir) = temp_store().await;
        let id = store.create_session("g", &serde_json::json!({})).await.unwrap();
        store.append_event(id, &log_event(id, 0, "a")).await.unwrap();
        assert!(store.append_event(id, &log_event(id, 0, "dup")).await.is_err());
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p otto-persistence sqlite::`
Expected: FAIL — `append_event`/`replay_since` panic with `unimplemented!`.

- [ ] **Step 3: Implement `append_event` and `replay_since`**

In the `impl crate::SessionStore for SqliteStore` block, replace the `append_event` and `replay_since` stub bodies with:

```rust
    async fn append_event(
        &self,
        session: otto_protocol::SessionId,
        event: &otto_protocol::Event,
    ) -> anyhow::Result<()> {
        let kind = serde_json::to_string(&event.kind)?;
        sqlx::query("INSERT INTO events (session_id, seq, kind) VALUES (?1, ?2, ?3)")
            .bind(session.0.to_string())
            .bind(event.seq as i64)
            .bind(kind)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
```

```rust
    async fn replay_since(
        &self,
        session: otto_protocol::SessionId,
        after_seq: u64,
    ) -> anyhow::Result<Vec<otto_protocol::Event>> {
        let rows: Vec<(i64, String)> = sqlx::query_as(
            "SELECT seq, kind FROM events
             WHERE session_id = ?1 AND seq > ?2
             ORDER BY seq ASC",
        )
        .bind(session.0.to_string())
        .bind(after_seq as i64)
        .fetch_all(&self.pool)
        .await?;
        let mut events = Vec::with_capacity(rows.len());
        for (seq, kind) in rows {
            events.push(otto_protocol::Event {
                seq: seq as u64,
                session,
                kind: serde_json::from_str(&kind)?,
            });
        }
        Ok(events)
    }
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p otto-persistence sqlite::`
Expected: PASS (all sqlite tests, including the 3 new ones).

- [ ] **Step 5: Commit**

```bash
git add crates/persistence/src/sqlite.rs
git commit -m "feat(persistence): append_event + gap-correct replay_since"
```

---

### Task 6: `set_status`

**Files:**
- Modify: `crates/persistence/src/sqlite.rs`

- [ ] **Step 1: Write the failing tests**

Add inside `mod tests`:

```rust
    #[tokio::test]
    async fn set_status_updates_existing_session() {
        let (store, _dir) = temp_store().await;
        let id = store.create_session("g", &serde_json::json!({})).await.unwrap();
        store.set_status(id, SessionStatus::Done).await.unwrap();
        assert_eq!(store.session_status(id).await.unwrap(), SessionStatus::Done);
    }

    #[tokio::test]
    async fn set_status_on_missing_session_is_error() {
        let (store, _dir) = temp_store().await;
        let missing = otto_protocol::SessionId::new();
        assert!(store.set_status(missing, SessionStatus::Aborted).await.is_err());
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p otto-persistence sqlite::set_status`
Expected: FAIL — `set_status` panics with `unimplemented!`.

- [ ] **Step 3: Implement `set_status`**

In the `impl crate::SessionStore for SqliteStore` block, replace the `set_status` stub body with:

```rust
    async fn set_status(
        &self,
        session: otto_protocol::SessionId,
        status: crate::SessionStatus,
    ) -> anyhow::Result<()> {
        let result = sqlx::query(
            "UPDATE sessions SET status = ?1, updated_at = ?2 WHERE id = ?3",
        )
        .bind(status.as_db_str())
        .bind(now_millis())
        .bind(session.0.to_string())
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            anyhow::bail!("set_status: no session {}", session.0);
        }
        Ok(())
    }
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p otto-persistence sqlite::set_status`
Expected: PASS (2 tests).

- [ ] **Step 5: Commit**

```bash
git add crates/persistence/src/sqlite.rs
git commit -m "feat(persistence): set_status with missing-session guard"
```

---

### Task 7: `record_turn`

**Files:**
- Modify: `crates/persistence/src/sqlite.rs`

- [ ] **Step 1: Write the failing tests**

Add inside `mod tests` (extend imports to include `TurnRecord`):

Update the `use crate::{...}` test import to:

```rust
    use crate::{SessionStatus, SessionStore, TurnRecord};
```

Add the tests:

```rust
    fn turn(idx: u32, ok: bool) -> TurnRecord {
        TurnRecord {
            turn_index: idx,
            goal: "g".to_string(),
            outcome: serde_json::json!({ "ok": ok }),
        }
    }

    #[tokio::test]
    async fn record_turn_succeeds() {
        let (store, _dir) = temp_store().await;
        let id = store.create_session("g", &serde_json::json!({})).await.unwrap();
        store.record_turn(id, &turn(0, true)).await.unwrap();
    }

    #[tokio::test]
    async fn record_turn_rejects_duplicate_index() {
        let (store, _dir) = temp_store().await;
        let id = store.create_session("g", &serde_json::json!({})).await.unwrap();
        store.record_turn(id, &turn(0, true)).await.unwrap();
        assert!(store.record_turn(id, &turn(0, false)).await.is_err());
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p otto-persistence sqlite::record_turn`
Expected: FAIL — `record_turn` panics with `unimplemented!`.

- [ ] **Step 3: Implement `record_turn`**

In the `impl crate::SessionStore for SqliteStore` block, replace the `record_turn` stub body with:

```rust
    async fn record_turn(
        &self,
        session: otto_protocol::SessionId,
        turn: &crate::TurnRecord,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO turns (session_id, turn_index, goal, outcome, started_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )
        .bind(session.0.to_string())
        .bind(turn.turn_index as i64)
        .bind(&turn.goal)
        .bind(serde_json::to_string(&turn.outcome)?)
        .bind(now_millis())
        .execute(&self.pool)
        .await?;
        Ok(())
    }
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p otto-persistence sqlite::record_turn`
Expected: PASS (2 tests).

- [ ] **Step 5: Commit**

```bash
git add crates/persistence/src/sqlite.rs
git commit -m "feat(persistence): record_turn"
```

---

### Task 8: Cross-session isolation test + full verification

**Files:**
- Modify: `crates/persistence/src/sqlite.rs`

- [ ] **Step 1: Write a test proving replay is scoped to one session**

Add inside `mod tests`:

```rust
    #[tokio::test]
    async fn replay_is_isolated_per_session() {
        let (store, _dir) = temp_store().await;
        let a = store.create_session("a", &serde_json::json!({})).await.unwrap();
        let b = store.create_session("b", &serde_json::json!({})).await.unwrap();
        store.append_event(a, &log_event(a, 0, "for-a")).await.unwrap();
        store.append_event(b, &log_event(b, 0, "for-b")).await.unwrap();
        assert_eq!(store.replay_since(a, 0).await.unwrap(), vec![log_event(a, 0, "for-a")]);
        assert_eq!(store.replay_since(b, 0).await.unwrap(), vec![log_event(b, 0, "for-b")]);
    }
```

- [ ] **Step 2: Run the full crate test suite**

Run: `cargo test -p otto-persistence`
Expected: PASS (all type + sqlite tests).

- [ ] **Step 3: Verify the whole workspace still builds, lints, and tests clean**

Run: `cargo fmt --all && cargo clippy -p otto-persistence --all-targets && cargo test --workspace`
Expected: fmt makes no complaint, clippy is clean (no warnings), and the full workspace test suite passes (the new crate added, nothing else regressed).

- [ ] **Step 4: Commit**

```bash
git add crates/persistence/src/sqlite.rs
git commit -m "test(persistence): cross-session replay isolation"
```

---

## Done criteria

- `otto-persistence` is a workspace member depending only on `otto-protocol` (+ sqlx/serde/anyhow/async-trait).
- `SessionStore` trait with `create_session`, `append_event`, `record_turn`, `set_status`, `replay_since`; `SqliteStore` implements all five.
- `replay_since(session, after_seq)` is gap-correct (returns only `seq > after_seq`, ascending) and session-scoped.
- Schema created idempotently at `open`; duplicate `(session, seq)` and `(session, turn_index)` rejected; `set_status`/`session_status` on a missing session error.
- Full suite offline and deterministic; `cargo test --workspace` green.

**Next:** Plan B — consume `CreateSession`/`SendPrompt`/`Abort`, persist events as they stream from a session-aware engine path, and refactor `run_goal` through the store.
