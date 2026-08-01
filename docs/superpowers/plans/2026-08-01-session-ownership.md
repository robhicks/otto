# Session Ownership Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give every session a `NOT NULL` owner and route every session-bearing operation through one authorization choke point, with **no observable behavior change**, so that slice 1b turns tenant isolation on by supplying real principals rather than by restructuring the store.

**Architecture:** A validated `UserId` newtype in `protocol`; an `owner` column on `sessions` guarded by a `PRAGMA user_version` probe (the crate has no migration mechanism, so a `NOT NULL` column added to an existing file would otherwise fail at query time); owner-scoped `SessionStore` reads whose ownership predicate lives *in the SQL statement* rather than in a caller; and an `EngineService::authorize` choke point. Every existing call site passes the reserved `UserId::local()`, so behavior is byte-for-byte unchanged.

**Tech Stack:** Rust (edition 2024, toolchain pinned stable, `rust-version = "1.85"`), `sqlx` 0.8 (sqlite, `runtime-tokio`), `serde`, `anyhow`, `tokio` + `tempfile` for tests. **No new dependency is added by this plan.**

**Spec:** `docs/superpowers/specs/2026-08-01-session-ownership-design.md` — read it first. This plan implements it exactly.

## Global Constraints

- **No new dependencies.** Not in any crate. If a task seems to need one, stop — it has drifted into slice 1b.
- **`ui-dioxus/` is not touched.** `git diff --stat` must show no file under it (spec success criterion 6). No `ServerMessage` variant is added or removed, so `ui-dioxus/src/app.rs:129-214`'s exhaustive match still compiles.
- **No new protocol wire variant.** `Command`, `EventKind`, and `ServerMessage` are unchanged. `UserId` is added as a type only; nothing puts it on the wire in this slice except inside `SessionState` (which `PromoteBundle` already serializes).
- **No authentication change.** `authorized`/`authorized_ws` in `crates/engine/src/serve.rs:73-85` are untouched, `?token=` stays, `OTTO_TOKEN` keeps its name and meaning. All of that is slice 1b.
- **Observable behavior is unchanged** (spec success criterion 2). `otto run` emits the same event sequence; `otto serve` authenticates identically.
- **Dependency flow stays strictly inward:** `protocol` → `engine-core` → impl crates → `engine`. `protocol` gains no dependency; `engine-core` is not modified by this plan at all.
- **The security spine is untouched:** no change to the sensitive-path floor, to gated fail-closed Coder edits, or to the `bash`-only-when-sandboxed rule.
- **Determinism holds:** no `OTTO_*` or API-key read is added anywhere; the suite stays offline and needs no keys.
- **No Claude/AI self-attribution** in any commit message, comment, or doc.
- Run `cargo fmt --all` from the repo root before **every** Rust commit; rustfmt is pinned in `rust-toolchain.toml`.
- **Known pre-existing failure:** `otto-mcp-lsp`'s `rust_analyzer_integration::full_round_trip_against_a_real_rust_analyzer` fails on `main` already (verified on `origin/main` f715522: 43 passed, 1 failed). It is not a regression from this work. Every "expect workspace green" step below means "green except that one test".

## File Structure

| File | Responsibility |
|---|---|
| `crates/protocol/src/user.rs` | **Create.** `UserId` newtype + `InvalidUserId` + validation + `local()`, with its own `#[cfg(test)] mod tests`. |
| `crates/protocol/src/lib.rs` | **Modify.** `mod user; pub use user::{InvalidUserId, UserId};` alongside the existing `sensitive` re-export at `:9-10`. |
| `crates/persistence/src/types.rs:52-60` | **Modify.** `SessionState` gains `pub owner: UserId`. |
| `crates/persistence/src/sqlite.rs:31-67` | **Modify.** `owner` column + index; `init_schema` becomes version-guarded. |
| `crates/persistence/src/sqlite.rs:70-358` | **Modify.** `create_session`, `owner_of`, and the three scoped reads. |
| `crates/persistence/src/lib.rs:17-68` | **Modify.** The `SessionStore` trait signatures + the doc note on unscoped methods. |
| `crates/engine/src/service.rs` | **Modify.** `authorize` choke point; `&UserId` on five client-facing methods; owner derived internally on the three machine-to-machine ones; ~24 test call sites. |
| `crates/engine/src/lib.rs:554-575` | **Modify.** `run_goal` passes `UserId::local()`. |
| `crates/engine/src/serve.rs:576,1055-1068` | **Modify.** Replay and `resolve_session` pass `UserId::local()`. |
| `crates/engine/tests/*.rs` | **Modify.** Seven harnesses, mechanically. |
| `CLAUDE.md` | **Modify.** The `persistence` crate-table row states ownership. |

## Task Order & Rationale

Forced by the inward dependency rule: `protocol` (Task 1) → `persistence` (Tasks 2–3) → `engine` (Tasks 4–5).

**An intermediate-state warning the implementer must expect:** changing the `SessionStore` trait in Task 3 breaks `crates/engine`, which is a *different crate*. So after Tasks 3, `cargo test -p otto-persistence` passes while `cargo build --workspace` does **not**. That is expected and stated per task. The workspace returns to green at the end of Task 5, and only Tasks 1, 2, and 5 end with a workspace-green commit. Do not "fix" the intermediate breakage by shimming old signatures — there is no installed base to preserve them for.

Task 2 lands the schema and `SessionState.owner` while `create_session` still has its old signature (defaulting the column to `local` internally), so persistence stays self-consistent and its 26 tests keep passing. Task 3 then changes the trait surface. Splitting them this way keeps each task's test cycle meaningful.

---

### Task 1: `UserId` in `protocol`

**Files:**
- Create: `crates/protocol/src/user.rs`
- Modify: `crates/protocol/src/lib.rs:9-10`

**Interfaces:**
- Consumes: nothing.
- Produces, used by every later task:
  - `pub struct UserId(String)` — `Debug + Clone + PartialEq + Eq + Hash + PartialOrd + Ord + Serialize + Deserialize`
  - `pub fn UserId::parse(s: &str) -> Result<UserId, InvalidUserId>`
  - `pub fn UserId::local() -> UserId` (the reserved `"local"` principal)
  - `pub fn UserId::as_str(&self) -> &str`
  - `pub struct InvalidUserId` — `Debug + Clone + PartialEq + Eq + Display + std::error::Error`
  - `impl From<UserId> for String`, `impl TryFrom<String> for UserId`

- [ ] **Step 1: Write the failing test**

Create `crates/protocol/src/user.rs` containing ONLY the test module for now:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_the_legal_charset() {
        for ok in ["alice", "a", "a.b_c-d", "user01", &"x".repeat(64)] {
            assert!(UserId::parse(ok).is_ok(), "should accept {ok:?}");
        }
    }

    #[test]
    fn parse_rejects_illegal_ids() {
        for bad in [
            "",              // empty
            &"x".repeat(65), // too long
            "Alice",         // uppercase
            "a b",           // space
            "../etc",        // path traversal characters
            "a'b",           // quote
            "a/b",           // separator
        ] {
            assert!(UserId::parse(bad).is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn local_is_the_reserved_principal() {
        assert_eq!(UserId::local().as_str(), "local");
    }

    #[test]
    fn round_trips_as_a_bare_string() {
        let id = UserId::parse("alice").unwrap();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"alice\"");
        assert_eq!(serde_json::from_str::<UserId>(&json).unwrap(), id);
    }

    /// The `try_from` guard is the point: a hostile `SessionState` arriving in a
    /// PromoteBundle must not be able to carry an owner that skipped validation.
    #[test]
    fn deserializing_an_illegal_id_fails() {
        assert!(serde_json::from_str::<UserId>("\"../etc/passwd\"").is_err());
        assert!(serde_json::from_str::<UserId>("\"\"").is_err());
    }
}
```

Add to `crates/protocol/src/lib.rs` immediately after the existing `pub mod sensitive;` line (`lib.rs:9`):

```rust
mod user;
pub use user::{InvalidUserId, UserId};
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p otto-protocol user::`
Expected: FAIL to compile — `cannot find type UserId in this scope`.

- [ ] **Step 3: Write the implementation**

Prepend to `crates/protocol/src/user.rs`, above the test module:

```rust
//! The principal that owns a session.
//!
//! Lives in `protocol` rather than in `persistence` because `SessionState` carries it, and
//! `SessionState` is serialized into a `PromoteBundle` and shipped between machines. Slice 1b
//! additionally puts it on the wire in `Credentials` and `LoggedIn`.

use serde::{Deserialize, Serialize};

/// The reserved principal that owns every session until slice 1b introduces real identities.
const LOCAL: &str = "local";

/// Maximum length of a `UserId`, in bytes.
const MAX_LEN: usize = 64;

/// A principal's stable identifier.
///
/// Validated on construction **and on deserialization**: 1–64 characters of `[a-z0-9._-]`. That
/// charset keeps it safe as a sqlite key, as an `otpauth://` URI label (slice 1b), and in a log
/// line without escaping.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct UserId(String);

impl UserId {
    /// Validate and construct. See the type docs for the accepted charset.
    pub fn parse(s: &str) -> Result<Self, InvalidUserId> {
        if s.is_empty() || s.len() > MAX_LEN {
            return Err(InvalidUserId);
        }
        if !s
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-'))
        {
            return Err(InvalidUserId);
        }
        Ok(Self(s.to_string()))
    }

    /// The reserved principal that owns sessions created by the offline CLI path and by every
    /// pre-identity `otto serve` connection. Never enrollable — slice 1b's `otto auth enroll`
    /// refuses it.
    pub fn local() -> Self {
        Self(LOCAL.to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for UserId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<UserId> for String {
    fn from(u: UserId) -> Self {
        u.0
    }
}

impl TryFrom<String> for UserId {
    type Error = InvalidUserId;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::parse(&s)
    }
}

/// A `UserId` that failed validation. Carries no detail: the rejected value is attacker-controlled
/// and echoing it into an error string invites log injection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidUserId;

impl std::fmt::Display for InvalidUserId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("invalid user id: expected 1-64 characters of [a-z0-9._-]")
    }
}

impl std::error::Error for InvalidUserId {}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p otto-protocol`
Expected: PASS, all tests including the pre-existing ones.

- [ ] **Step 5: Verify the workspace still builds**

Run: `cargo build --workspace`
Expected: SUCCESS — nothing consumes `UserId` yet.

- [ ] **Step 6: Format and commit**

```bash
cargo fmt --all
git add crates/protocol/src/user.rs crates/protocol/src/lib.rs
git commit -m "protocol: add the validated UserId principal newtype"
```

---

### Task 2: The `owner` column and the `user_version` guard

**Files:**
- Modify: `crates/persistence/src/sqlite.rs:31-67` (`init_schema`), `:72-92` (`create_session`), `:187-227` (`snapshot`)
- Modify: `crates/persistence/src/types.rs:52-60` (`SessionState`)

**Interfaces:**
- Consumes: `otto_protocol::UserId` from Task 1.
- Produces, used by Task 3: the `owner` column on `sessions`; `SessionState { owner: UserId, .. }`; `const SCHEMA_VERSION: i64 = 1`.

**Note:** the `SessionStore` trait signatures do NOT change in this task. `create_session` keeps `(goal, config)` and writes `UserId::local()` into the new column, so `persistence` stays self-consistent and the workspace stays green. Task 3 changes the surface.

- [ ] **Step 1: Write the failing tests**

Append to the `mod tests` block in `crates/persistence/src/sqlite.rs` (it ends at `:759`):

```rust
    #[tokio::test]
    async fn create_session_defaults_the_owner_to_local() {
        let (store, _dir) = temp_store().await;
        let id = store
            .create_session("g", &serde_json::json!({}))
            .await
            .unwrap();
        let owner: (String,) = sqlx::query_as("SELECT owner FROM sessions WHERE id = ?1")
            .bind(id.0.to_string())
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(owner.0, "local");
    }

    #[tokio::test]
    async fn snapshot_carries_the_owner() {
        let (store, _dir) = temp_store().await;
        let id = store
            .create_session("g", &serde_json::json!({}))
            .await
            .unwrap();
        let state = store.snapshot(id).await.unwrap();
        assert_eq!(state.owner, otto_protocol::UserId::local());
    }

    #[tokio::test]
    async fn fresh_database_is_stamped_with_the_schema_version() {
        let (store, _dir) = temp_store().await;
        let v: (i64,) = sqlx::query_as("PRAGMA user_version")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(v.0, SCHEMA_VERSION);
    }

    /// A pre-ownership database must fail loudly at open, not at some later query with a
    /// confusing "no such column: owner". There is no installed base, so there is no migration.
    #[tokio::test]
    async fn opening_a_pre_ownership_database_is_a_clear_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy.db");

        // Build the OLD schema by hand: `sessions` with no `owner`, user_version left at 0.
        {
            let opts = SqliteConnectOptions::new()
                .filename(&path)
                .create_if_missing(true);
            let pool = SqlitePoolOptions::new().connect_with(opts).await.unwrap();
            sqlx::query(
                "CREATE TABLE sessions (
                    id TEXT PRIMARY KEY, goal TEXT NOT NULL, status TEXT NOT NULL,
                    created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL, config TEXT NOT NULL
                )",
            )
            .execute(&pool)
            .await
            .unwrap();
            pool.close().await;
        }

        let err = SqliteStore::open(&path).await.unwrap_err().to_string();
        assert!(err.contains("predates session ownership"), "unexpected: {err}");
        assert!(err.contains("delete the file"), "unexpected: {err}");
    }

    /// sqlite will happily let an older binary open a newer file; refuse instead of corrupting it.
    #[tokio::test]
    async fn opening_a_forward_version_database_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("future.db");
        {
            let store = SqliteStore::open(&path).await.unwrap();
            sqlx::query(&format!("PRAGMA user_version = {}", SCHEMA_VERSION + 1))
                .execute(&store.pool)
                .await
                .unwrap();
            store.pool.close().await;
        }
        let err = SqliteStore::open(&path).await.unwrap_err().to_string();
        assert!(err.contains("newer than this otto build"), "unexpected: {err}");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p otto-persistence`
Expected: FAIL to compile — `SCHEMA_VERSION` not found, `state.owner` no such field.

- [ ] **Step 3: Add `owner` to `SessionState`**

In `crates/persistence/src/types.rs`, add the field to `SessionState` (currently `:52-60`), immediately after `pub id: SessionId,`:

```rust
    /// The principal that owns this session. Travels with the session across a promote, so a
    /// restored session belongs to whoever owned it on the source — not to whoever pushed it.
    pub owner: otto_protocol::UserId,
```

- [ ] **Step 4: Add the version guard and the column**

In `crates/persistence/src/sqlite.rs`, add near the top (after the `use` block at `:5-9`):

```rust
/// Bumped whenever the on-disk schema changes shape. Stamped into `PRAGMA user_version`.
const SCHEMA_VERSION: i64 = 1;
```

Replace `init_schema`'s body (`:31-67`) so the probe runs first. Keep the three `CREATE TABLE`
statements exactly as they are apart from the new `owner` column and index:

```rust
    /// Create the schema on a fresh database, or verify an existing one is the shape we expect.
    ///
    /// The crate has no migration mechanism — `CREATE TABLE IF NOT EXISTS` cannot alter an
    /// existing table — so a pre-ownership file would silently keep its old shape and then fail
    /// at query time with "no such column: owner". The probe turns that into one clear error at
    /// open. `PRAGMA user_version` is 0 on both a brand-new file and every pre-ownership one;
    /// the two are told apart by whether `sessions` already exists.
    async fn init_schema(&self) -> anyhow::Result<()> {
        let (user_version,): (i64,) = sqlx::query_as("PRAGMA user_version")
            .fetch_one(&self.pool)
            .await?;
        let sessions_exists: Option<(String,)> =
            sqlx::query_as("SELECT name FROM sqlite_master WHERE type='table' AND name='sessions'")
                .fetch_optional(&self.pool)
                .await?;

        match (user_version, sessions_exists.is_some()) {
            (0, false) => {}
            (0, true) => anyhow::bail!(
                "session database predates session ownership (issue #115) and has no owner \
                 column. otto has no installed base, so there is no migration: delete the file \
                 and let otto re-create it."
            ),
            (v, _) if v == SCHEMA_VERSION => return Ok(()),
            (v, _) => anyhow::bail!(
                "session database has schema version {v}, newer than this otto build \
                 understands ({SCHEMA_VERSION}); upgrade otto"
            ),
        }

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS sessions (
                id TEXT PRIMARY KEY,
                owner TEXT NOT NULL,
                goal TEXT NOT NULL,
                status TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                config TEXT NOT NULL
            )",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS sessions_owner_idx ON sessions (owner)")
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

        sqlx::query(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))
            .execute(&self.pool)
            .await?;
        Ok(())
    }
```

- [ ] **Step 5: Write the owner on insert and read it back on snapshot**

In `create_session` (`:79-90`), add the column and bind `UserId::local()`:

```rust
        sqlx::query(
            "INSERT INTO sessions (id, owner, goal, status, created_at, updated_at, config)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )
        .bind(id.0.to_string())
        .bind(otto_protocol::UserId::local().as_str())
        .bind(goal)
        .bind(crate::SessionStatus::Active.as_db_str())
        .bind(now)
        .bind(now)
        .bind(serde_json::to_string(config)?)
        .execute(&self.pool)
        .await?;
```

In `snapshot` (`:191-197`), select `owner` and parse it:

```rust
        let row: Option<(String, String, String, String)> =
            sqlx::query_as("SELECT owner, goal, status, config FROM sessions WHERE id = ?1")
                .bind(session.0.to_string())
                .fetch_optional(&self.pool)
                .await?;
        let (owner, goal, status, config) =
            row.ok_or_else(|| anyhow::anyhow!("snapshot: no session {}", session.0))?;
        let owner = otto_protocol::UserId::parse(&owner)
            .map_err(|e| anyhow::anyhow!("snapshot: stored owner is invalid: {e}"))?;
```

and add `owner,` to the returned `SessionState` literal (`:219-226`).

In `restore` (`:248-295`) and `restore_over` (`:297-358`), add `owner` to the sessions INSERT,
binding `state.owner.as_str()`. Both statements currently list
`(id, goal, status, created_at, updated_at, config)`; add `owner` in the same position the
`create_session` insert uses.

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test -p otto-persistence`
Expected: PASS — the five new tests plus all 26 pre-existing ones.

- [ ] **Step 7: Verify the workspace still builds**

Run: `cargo build --workspace`
Expected: SUCCESS. `SessionState` gained a field, so any struct-literal construction outside
`persistence` would break — if the build fails here, fix those call sites now rather than deferring.

- [ ] **Step 8: Format and commit**

```bash
cargo fmt --all
git add crates/persistence/src
git commit -m "persistence: add the session owner column behind a schema-version guard"
```

---

### Task 3: Owner-scoped `SessionStore` reads

**Files:**
- Modify: `crates/persistence/src/lib.rs:17-68` (the trait)
- Modify: `crates/persistence/src/sqlite.rs` (`create_session`, `owner_of`, `replay_since`, `session_status`, `snapshot`)

**Interfaces:**
- Consumes: Task 2's column; Task 1's `UserId`.
- Produces, used by Tasks 4–5:
  - `async fn create_session(&self, owner: &UserId, goal: &str, config: &Value) -> anyhow::Result<SessionId>`
  - `async fn owner_of(&self, session: SessionId) -> anyhow::Result<UserId>`
  - `async fn replay_since(&self, owner: &UserId, session: SessionId, after_seq: Option<u64>) -> anyhow::Result<Vec<Event>>`
  - `async fn session_status(&self, owner: &UserId, session: SessionId) -> anyhow::Result<SessionStatus>`
  - `async fn snapshot(&self, owner: &UserId, session: SessionId) -> anyhow::Result<SessionState>`

**Behavioral note that decides two of the tests below.** `replay_since` today returns `Ok(vec![])`
for a nonexistent session — it does not error. Scoping it with a join therefore returns an empty
vec for *both* wrong-owner and nonexistent, which is simultaneously leak-free and behavior-preserving.
`session_status` and `snapshot` do error today, so those two are the ones that must return a
byte-identical message for wrong-owner and nonexistent.

- [ ] **Step 1: Write the failing tests**

Append to `mod tests` in `crates/persistence/src/sqlite.rs`:

```rust
    fn alice() -> otto_protocol::UserId {
        otto_protocol::UserId::parse("alice").unwrap()
    }
    fn bob() -> otto_protocol::UserId {
        otto_protocol::UserId::parse("bob").unwrap()
    }

    #[tokio::test]
    async fn create_session_records_the_given_owner() {
        let (store, _dir) = temp_store().await;
        let id = store
            .create_session(&alice(), "g", &serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(store.owner_of(id).await.unwrap(), alice());
    }

    #[tokio::test]
    async fn scoped_reads_succeed_for_the_owner() {
        let (store, _dir) = temp_store().await;
        let id = store
            .create_session(&alice(), "g", &serde_json::json!({}))
            .await
            .unwrap();
        store.append_event(id, &log_event(id, 0, "hi")).await.unwrap();

        assert_eq!(store.replay_since(&alice(), id, None).await.unwrap().len(), 1);
        assert_eq!(
            store.session_status(&alice(), id).await.unwrap(),
            SessionStatus::Active
        );
        assert_eq!(store.snapshot(&alice(), id).await.unwrap().owner, alice());
    }

    /// The API must not be an existence oracle: "someone else's session" and "no such session"
    /// must be indistinguishable. Asserting string equality, not just `is_err`.
    #[tokio::test]
    async fn wrong_owner_is_byte_identical_to_nonexistent() {
        let (store, _dir) = temp_store().await;
        let id = store
            .create_session(&alice(), "g", &serde_json::json!({}))
            .await
            .unwrap();
        let missing = otto_protocol::SessionId::new();

        let wrong = store.session_status(&bob(), id).await.unwrap_err().to_string();
        let absent = store
            .session_status(&bob(), missing)
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(
            wrong.replace(&id.0.to_string(), "ID"),
            absent.replace(&missing.0.to_string(), "ID"),
            "session_status must not distinguish wrong-owner from nonexistent"
        );

        let wrong = store.snapshot(&bob(), id).await.unwrap_err().to_string();
        let absent = store.snapshot(&bob(), missing).await.unwrap_err().to_string();
        assert_eq!(
            wrong.replace(&id.0.to_string(), "ID"),
            absent.replace(&missing.0.to_string(), "ID"),
            "snapshot must not distinguish wrong-owner from nonexistent"
        );
    }

    /// replay_since returns an empty vec for a nonexistent session today; a wrong owner must
    /// look exactly the same, and must not leak the events.
    #[tokio::test]
    async fn replay_for_the_wrong_owner_is_empty() {
        let (store, _dir) = temp_store().await;
        let id = store
            .create_session(&alice(), "g", &serde_json::json!({}))
            .await
            .unwrap();
        store.append_event(id, &log_event(id, 0, "secret")).await.unwrap();

        assert!(store.replay_since(&bob(), id, None).await.unwrap().is_empty());
        assert!(
            store
                .replay_since(&bob(), otto_protocol::SessionId::new(), None)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn restore_preserves_the_owner() {
        let (src, _d1) = temp_store().await;
        let id = src
            .create_session(&alice(), "g", &serde_json::json!({}))
            .await
            .unwrap();
        let state = src.snapshot(&alice(), id).await.unwrap();

        let (dst, _d2) = temp_store().await;
        dst.restore(&state).await.unwrap();
        assert_eq!(dst.owner_of(id).await.unwrap(), alice());
    }
```

**Also update every pre-existing test in this module** that calls `create_session`, `replay_since`,
`session_status`, or `snapshot` to pass an owner — use `&alice()` for new-style tests or
`&otto_protocol::UserId::local()` where the test does not care. There are 26 tests in
`sqlite.rs:369-759`; the compiler will name each one.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p otto-persistence`
Expected: FAIL to compile — `owner_of` not found; wrong argument counts.

- [ ] **Step 3: Change the trait**

In `crates/persistence/src/lib.rs`, update the five signatures and add the doc note. Replace
`create_session` (`:21`), `replay_since` (`:37-41`), `session_status` (`:44`), and `snapshot` (`:56`),
and add `owner_of`:

```rust
    /// Create a session owned by `owner`.
    async fn create_session(
        &self,
        owner: &otto_protocol::UserId,
        goal: &str,
        config: &Value,
    ) -> anyhow::Result<SessionId>;

    /// The owner of `session`, or `Err` if there is no such session.
    ///
    /// UNSCOPED, and deliberately a reverse existence oracle: it distinguishes "exists, owned by
    /// someone else" from "does not exist" — exactly the distinction the scoped reads below
    /// hide. `EngineService::authorize` needs that comparison, so it cannot be removed. It must
    /// therefore NEVER back a client-facing path.
    async fn owner_of(&self, session: SessionId) -> anyhow::Result<otto_protocol::UserId>;
```

Add to the trait's doc comment (above `pub trait SessionStore`, `lib.rs:17`):

```rust
/// # Which methods are owner-scoped, and why not all of them
///
/// `create_session`, `owner_of`, `replay_since`, `session_status`, and `snapshot` take a
/// principal: they are the methods a client can reach with a session id it does not own, and the
/// ones that would return another tenant's data. Their ownership predicate lives inside the SQL
/// statement, so no caller can forget it.
///
/// `append_event`, `record_turn`, `next_seq`, `next_turn`, and `set_status` are deliberately
/// UNSCOPED. They are reachable only from inside a turn that `EngineService` has already
/// authorized, none of them returns another tenant's data, and scoping them would roughly triple
/// the churn for no gain. This is a deliberate trade, not an oversight.
```

- [ ] **Step 4: Implement the scoped reads**

In `crates/persistence/src/sqlite.rs`:

Add a single shared error constructor near `now_millis` (`:362`), so the two paths cannot drift:

```rust
/// The error returned for BOTH "no such session" and "session owned by someone else".
///
/// One constructor, deliberately: if these two ever produced different strings the API would
/// become an existence oracle for other tenants' session ids.
fn no_such_session(op: &str, session: otto_protocol::SessionId) -> anyhow::Error {
    anyhow::anyhow!("{op}: no session {}", session.0)
}
```

`create_session` — take `owner: &otto_protocol::UserId` and bind `owner.as_str()` in place of the
`UserId::local()` literal added in Task 2.

`owner_of`:

```rust
    async fn owner_of(
        &self,
        session: otto_protocol::SessionId,
    ) -> anyhow::Result<otto_protocol::UserId> {
        let row: Option<(String,)> = sqlx::query_as("SELECT owner FROM sessions WHERE id = ?1")
            .bind(session.0.to_string())
            .fetch_optional(&self.pool)
            .await?;
        let (owner,) = row.ok_or_else(|| no_such_session("owner_of", session))?;
        Ok(otto_protocol::UserId::parse(&owner)
            .map_err(|e| anyhow::anyhow!("owner_of: stored owner is invalid: {e}"))?)
    }
```

`replay_since` — join to `sessions` so a wrong owner yields no rows:

```rust
        let rows: Vec<(i64, String)> = sqlx::query_as(
            "SELECT e.seq, e.kind FROM events e
             JOIN sessions s ON s.id = e.session_id
             WHERE e.session_id = ?1 AND s.owner = ?2 AND e.seq > ?3
             ORDER BY e.seq ASC",
        )
        .bind(session.0.to_string())
        .bind(owner.as_str())
        .bind(bound)
        .fetch_all(&self.pool)
        .await?;
```

`session_status` — add `AND owner = ?2` and use the shared error:

```rust
        let row: Option<(String,)> =
            sqlx::query_as("SELECT status FROM sessions WHERE id = ?1 AND owner = ?2")
                .bind(session.0.to_string())
                .bind(owner.as_str())
                .fetch_optional(&self.pool)
                .await?;
        let (status,) = row.ok_or_else(|| no_such_session("session_status", session))?;
```

`snapshot` — same treatment, and pass the owner through to its internal `replay_since` call
(`:201`, which becomes `self.replay_since(owner, session, None)`):

```rust
        let row: Option<(String, String, String, String)> = sqlx::query_as(
            "SELECT owner, goal, status, config FROM sessions WHERE id = ?1 AND owner = ?2",
        )
        .bind(session.0.to_string())
        .bind(owner.as_str())
        .fetch_optional(&self.pool)
        .await?;
        let (owner_str, goal, status, config) =
            row.ok_or_else(|| no_such_session("snapshot", session))?;
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p otto-persistence`
Expected: PASS — all tests including the six new ones.

- [ ] **Step 6: Confirm the expected intermediate breakage**

Run: `cargo build -p otto-engine`
Expected: **FAILS** with argument-count errors at the `SessionStore` call sites. This is the
intermediate state described in Task Order & Rationale. Task 4 fixes it. Do not add compatibility
shims.

- [ ] **Step 7: Format and commit**

```bash
cargo fmt --all
git add crates/persistence/src
git commit -m "persistence: scope session reads by owner"
```

---

### Task 4: The `EngineService` authorization choke point

**Files:**
- Modify: `crates/engine/src/service.rs` (methods + ~24 test call sites)
- Modify: `crates/engine/src/lib.rs:554-575` (`run_goal`)

**Interfaces:**
- Consumes: Task 3's trait.
- Produces, used by Task 5:
  - `EngineService::create_session(&self, owner: &UserId, goal: &str, config: &Value)`
  - `EngineService::run_prompt(&self, owner: &UserId, session: SessionId, goal: &str, sink: &mut dyn EventSink)`
  - `run_prompt_with_controls`, `run_command_with_controls`, `run_agent_with_controls`, `abort` — each gaining a leading `owner: &UserId`
  - `accept_promotion`, `accept_demotion`, `export_promotion` — signatures **unchanged**

- [ ] **Step 1: Write the failing test**

Append to `mod tests` in `crates/engine/src/service.rs`:

```rust
    #[tokio::test]
    async fn client_facing_methods_reject_a_non_owner() {
        let (service, _dir) = test_service().await;
        let alice = otto_protocol::UserId::parse("alice").unwrap();
        let bob = otto_protocol::UserId::parse("bob").unwrap();
        let session = service
            .create_session(&alice, "g", &serde_json::json!({}))
            .await
            .unwrap();

        let mut sink = CollectingSink::default();
        let err = service
            .run_prompt(&bob, session, "go", &mut sink)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("no session"), "unexpected: {err}");
        assert!(sink.events.is_empty(), "a rejected turn must emit nothing");

        assert!(service.abort(&bob, session).await.is_err());
        // ...and the owner still works.
        assert!(service.abort(&alice, session).await.is_ok());
    }

    /// export_promotion is machine-to-machine: it derives the owner itself and takes no
    /// principal, so there is no tautological "check" to pass.
    #[tokio::test]
    async fn export_promotion_needs_no_principal() {
        let (service, _dir) = test_service().await;
        let alice = otto_protocol::UserId::parse("alice").unwrap();
        let session = service
            .create_session(&alice, "g", &serde_json::json!({}))
            .await
            .unwrap();
        let bundle = service.export_promotion(session).await.unwrap();
        assert_eq!(bundle.session.owner, alice);
    }
```

If no `test_service()` helper exists in that module, build the service inline in the same shape the
neighbouring tests already use — match the surrounding style rather than inventing a new fixture.

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p otto-engine --lib service::`
Expected: FAIL to compile — wrong argument counts.

- [ ] **Step 3: Add the choke point and thread the principal**

In `crates/engine/src/service.rs`, add the private helper to `impl EngineService`:

```rust
    /// The single authorization choke point: every client-facing method calls this before
    /// touching a session. Returns the same error a nonexistent session produces, so a caller
    /// cannot tell "not yours" from "not there".
    async fn authorize(
        &self,
        owner: &otto_protocol::UserId,
        session: otto_protocol::SessionId,
    ) -> anyhow::Result<()> {
        match self.store.owner_of(session).await {
            Ok(actual) if &actual == owner => Ok(()),
            // Both arms produce the same message on purpose.
            _ => anyhow::bail!("no session {}", session.0),
        }
    }
```

Add a leading `owner: &otto_protocol::UserId` parameter to `create_session` (`:138`), `abort`
(`:147`), `run_prompt` (`:152`), `run_prompt_with_controls` (`:165`), `run_command_with_controls`
(`:276`), and `run_agent_with_controls` (`:339`). Each of the latter five calls
`self.authorize(owner, session).await?;` as its **first** statement — before any store write, any
event emission, and any turn-lock acquisition. `create_session` forwards the owner to the store.

`accept_promotion` (`:585`), `accept_demotion` (`:621`), and `export_promotion` (`:737`) are
machine-to-machine: they take **no** owner parameter. `export_promotion` derives one for the now
owner-scoped `snapshot`:

```rust
    pub async fn export_promotion(
        &self,
        session: otto_protocol::SessionId,
    ) -> anyhow::Result<otto_remote::PromoteBundle> {
        // Machine-to-machine: there is no connected principal. The owner is derived from the
        // session purely to satisfy the owner-scoped `snapshot`; passing it back in as an
        // authorization check would be a tautology. The real ownership check for handover
        // happens on the source, in the WS command loop.
        let owner = self.store.owner_of(session).await?;
        Ok(otto_remote::PromoteBundle {
            session: self.store.snapshot(&owner, session).await?,
            workspace: self.filtered_workspace_snapshot().await?,
        })
    }
```

`accept_promotion`'s duplicate probe at `:595` uses `session_status`, which is now scoped — read the
owner from `bundle.session.owner` and pass it.

In `crates/engine/src/lib.rs`, `run_goal` (`:554-575`) passes the reserved principal:

```rust
    let owner = otto_protocol::UserId::local();
    let session = service.create_session(&owner, goal, &session_config()).await?;
    let mut sink = CollectingSink::default();
    let outcome = service.run_prompt(&owner, session, goal, &mut sink).await?;
```

- [ ] **Step 4: Update the ~24 test call sites in `service.rs`**

Every `create_session`/`run_prompt`/`abort` call in that module's `#[cfg(test)]` block needs a
principal. Use `otto_protocol::UserId::local()` unless the test is specifically about ownership. The
compiler lists each one; work through them until `cargo test -p otto-engine --lib` compiles.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p otto-engine --lib`
Expected: PASS.

- [ ] **Step 6: Format and commit**

```bash
cargo fmt --all
git add crates/engine/src/service.rs crates/engine/src/lib.rs
git commit -m "engine: add the session authorization choke point"
```

---

### Task 5: `serve.rs`, the integration harnesses, and docs

**Files:**
- Modify: `crates/engine/src/serve.rs:576` (replay), `:1055-1068` (`resolve_session`)
- Modify: `crates/engine/tests/{serve,cors,ui_dir,promote,remote_workspace,vps_promote,microvm}.rs`
- Modify: `CLAUDE.md` (the `persistence` crate-table row)

**Interfaces:**
- Consumes: Task 4's `EngineService`.
- Produces: a workspace-green tree.

- [ ] **Step 1: Write the failing test**

Append to `crates/engine/tests/serve.rs`:

```rust
/// Ownership is wired end to end: a served session is owned by the reserved local principal,
/// and the store's scoped reads accept it. This is the seam slice 1b flips on — until then
/// there is exactly one principal, so this asserts the plumbing, not isolation.
#[tokio::test]
async fn served_sessions_are_owned_by_the_local_principal() {
    let (port, dir) = start_server().await;
    let (mut ws, _) = tokio_tungstenite::connect_async(authed_request(port, None)).await.unwrap();
    let ready: serde_json::Value = next_json(&mut ws).await;
    assert_eq!(ready["type"], "ready");
    let session = ready["session"].as_str().unwrap().to_string();

    let store = otto_persistence::SqliteStore::open(dir.path().join("s.db")).await.unwrap();
    let id = otto_protocol::SessionId(session.parse().unwrap());
    assert_eq!(
        otto_persistence::SessionStore::owner_of(&store, id).await.unwrap(),
        otto_protocol::UserId::local()
    );
}
```

Match the file's existing helper names (`start_server`, `authed_request`, `next_json` at
`tests/serve.rs:36`, `:532`, `:1139`); adjust if the harness signature differs.

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p otto-engine --test serve`
Expected: FAIL to compile — the harness itself does not build yet against Task 4's signatures.

- [ ] **Step 3: Update `serve.rs`**

`resolve_session` (`:1055-1068`) and the replay call (`:576`) pass `UserId::local()`. Add a comment
at `resolve_session` recording why:

```rust
    // One principal exists until slice 1b adds identity, so every served session is owned by
    // `local` and the ownership check below always passes. The check is wired now so that slice
    // 1b turns isolation on by supplying a real principal here — not by restructuring this.
    let owner = otto_protocol::UserId::local();
```

Where `resolve_session` accepts a client-supplied `?session=` uuid, verify ownership through the
service rather than attaching blind, so the seam is real rather than notional.

**Do not touch** `authorized`/`authorized_ws` (`:73-85`), `ConnectParams` (`:39-50`), or any route
definition. Authentication is slice 1b.

- [ ] **Step 4: Update the seven integration harnesses**

Mechanically add `&otto_protocol::UserId::local()` to the `create_session`/`run_prompt`/`abort`/
`snapshot`/`replay_since`/`session_status` calls in
`crates/engine/tests/{serve,cors,ui_dir,promote,remote_workspace,vps_promote,microvm}.rs`.
`promote.rs:72,80` and `vps_promote.rs:600,608` are the `SessionStore` call sites the survey found;
the compiler will name the rest.

If any harness needs a **semantic** change rather than an extra argument, stop and report it — that
is a signal the slice has grown beyond its scope.

- [ ] **Step 5: Run the full suite**

Run: `cargo test --workspace`
Expected: PASS, except the known pre-existing `otto-mcp-lsp` rust-analyzer failure.

- [ ] **Step 6: Lint and format check**

Run: `cargo clippy --workspace --all-targets` then `cargo fmt --all --check`
Expected: both clean.

- [ ] **Step 7: Confirm `ui-dioxus/` is untouched**

Run: `git diff --stat origin/main -- ui-dioxus/`
Expected: **empty output** (spec success criterion 6).

- [ ] **Step 8: Update `CLAUDE.md`**

In the crate table, extend the `persistence` row to state that sessions are owned: `SessionStore`
takes a principal on `create_session` and scopes `replay_since`/`session_status`/`snapshot` by
owner; the schema is guarded by `PRAGMA user_version`. Keep it to the table's existing one-cell
style. Do not claim otto is multitenant — it is not yet.

- [ ] **Step 9: Format and commit**

```bash
cargo fmt --all
git add crates/engine CLAUDE.md
git commit -m "engine: thread the session owner through serve and the test harnesses"
```

---

## Self-Review

**Spec coverage.** §1 `UserId` → Task 1. §2.1 schema + guard → Task 2. §2.2 `SessionState.owner` →
Task 2. §2.3 trait + scoped reads + `owner_of` → Task 3. §3.1 client-facing choke point → Task 4.
§3.2 machine-to-machine → Task 4. §3.3 call sites → Tasks 4–5. §5 testing → every task's test steps.
Success criteria 1/6 → Task 5 steps 5–7; 2 → the Global Constraints plus Task 5 step 7; 3 → Task 3
step 1; 4 → Task 3's `restore_preserves_the_owner`; 5 → Task 2's two guard tests.

**Type consistency.** `UserId::parse`/`local`/`as_str` are used with those exact names in Tasks 2–5.
`no_such_session(op, session)` is defined once in Task 3 and used by `owner_of`/`session_status`/
`snapshot`. `SCHEMA_VERSION` is defined in Task 2 and used by Task 2's tests. Every scoped method
takes `owner` **first**, matching the trait in Task 3 step 3.

**Known gap, deliberate:** Task 5 step 3 says `resolve_session` should verify ownership "through the
service" without pinning the exact call, because the right shape depends on whether the handler
holds an `EngineService` or a bare store at that point. The implementer picks the one that compiles
without widening a public surface, and reports which.
