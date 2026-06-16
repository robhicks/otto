# Persistence Lifecycle + Engine Wiring (Plan B) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Wire the `otto-persistence` store into the engine: introduce a `Session` type that creates a session, runs orchestrator turns while persisting their events/turn-records/status, supports multiple turns with a continuous seq counter, and can be aborted — then refactor `run_goal` to run through it.

**Architecture:** A new `crates/engine/src/session.rs` defines `Session<'a>`, holding only the persistent state (`store`, `id`, `next_seq`, `next_turn`). `Session::create` persists a new session row (≙ `Command::CreateSession`); `run_prompt` runs one `Orchestrator` turn, collects its events synchronously (the `Emitter` is sync), then persists the batch fail-closed and records the turn + status (≙ `Command::SendPrompt`); `abort` sets status `Aborted` (≙ `Command::Abort`). `run_goal` becomes a thin wrapper that creates a session and runs one prompt. Events are persisted at turn boundaries — true per-event streaming (channel + writer task) is deferred until a `serve` transport has a live client to observe it.

**Tech Stack:** Rust (edition 2024), `otto-persistence` (`SessionStore`/`SqliteStore`), `otto-engine-core` (`Orchestrator`), `otto-protocol` (`Event`/`EventKind`/`SessionId`), `serde_json`, tokio/tempfile for tests.

**Scope notes:**
- The literal `Command` enum (`CreateSession`/`SendPrompt`/`Abort`) is *mapped* onto the `Session` API but not dispatched from a wire here — command dispatch arrives with `serve`.
- `sessions.config` is captured by a new `session_config()` helper that snapshots the provider-selection env (the same env `build_router` reads), satisfying Plan A's "config fed real data in Plan B" note. A structured engine-config type is out of scope.

---

### Task 1: Add the `otto-persistence` dependency to the engine

**Files:**
- Modify: `crates/engine/Cargo.toml`

- [ ] **Step 1: Add the dependency**

In `crates/engine/Cargo.toml`, under `[dependencies]`, add the line after the other `otto-*` path deps (e.g. after the `otto-tools` line):

```toml
otto-persistence = { path = "../persistence" }
```

- [ ] **Step 2: Verify it builds**

Run: `cargo build -p otto-engine`
Expected: PASS (engine now links `otto-persistence`; no code uses it yet).

- [ ] **Step 3: Commit**

```bash
git add crates/engine/Cargo.toml Cargo.lock
git commit -m "build(engine): depend on otto-persistence"
```

---

### Task 2: `Session::create` + `session_config()`

**Files:**
- Create: `crates/engine/src/session.rs`
- Modify: `crates/engine/src/lib.rs`

- [ ] **Step 1: Create the session module with the failing test**

Create `crates/engine/src/session.rs`:

```rust
//! Session lifecycle over the persistence store. A `Session` owns the durable session
//! identity and the per-session monotonic `seq`/turn counters; running a prompt drives one
//! orchestrator turn and persists its events, turn record, and status. This maps the
//! protocol commands onto store operations: `CreateSession` -> `create`, `SendPrompt` ->
//! `run_prompt`, `Abort` -> `abort`. (Wire-level `Command` dispatch arrives with `serve`.)

use std::sync::{Arc, Mutex};

use otto_engine_core::traits::Workspace;
use otto_engine_core::{AgentRegistry, Orchestrator, Router, TurnOutcome};
use otto_persistence::{SessionStatus, SessionStore, TurnRecord};
use otto_protocol::{Event, EventKind, SessionId};

/// Persistent state for one session. Borrows the store; the engine deps for running a turn
/// are passed to `run_prompt` rather than held, so a `Session` is cheap to create and test.
pub struct Session<'a> {
    store: &'a dyn SessionStore,
    id: SessionId,
    next_seq: u64,
    next_turn: u32,
}

impl<'a> Session<'a> {
    /// Create and persist a new session for `goal` with `config` (provider selection as
    /// JSON). Status starts `Active`. (≙ `Command::CreateSession`.)
    pub async fn create(
        store: &'a dyn SessionStore,
        goal: &str,
        config: &serde_json::Value,
    ) -> anyhow::Result<Session<'a>> {
        let id = store.create_session(goal, config).await?;
        Ok(Session {
            store,
            id,
            next_seq: 0,
            next_turn: 0,
        })
    }

    /// The session's id.
    pub fn id(&self) -> SessionId {
        self.id
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use otto_persistence::SqliteStore;

    async fn store_in(dir: &tempfile::TempDir) -> SqliteStore {
        SqliteStore::open(dir.path().join("sessions.db"))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn create_persists_active_session() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(&dir).await;
        let session = Session::create(&store, "do a thing", &serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(
            store.session_status(session.id()).await.unwrap(),
            SessionStatus::Active
        );
    }
}
```

Note: `Arc`/`Mutex`, `Workspace`, `Orchestrator`, `Router`, `TurnOutcome`, `Event`, `EventKind`, `TurnRecord` are imported now but unused until Task 3; that is expected and they are used by `run_prompt` there. If an unused-import warning blocks a `-D warnings` build before Task 3, that is resolved in Task 3 — do not delete the imports.

- [ ] **Step 2: Register the module in `lib.rs`**

In `crates/engine/src/lib.rs`, add the module declaration and re-export near the top (after the `use` statements, before `build_default_registry`):

```rust
mod session;

pub use session::Session;
```

- [ ] **Step 3: Run the test to verify it passes**

Run: `cargo test -p otto-engine session::tests::create_persists_active_session`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/engine/src/session.rs crates/engine/src/lib.rs
git commit -m "feat(engine): Session::create persists a new session"
```

---

### Task 3: `Session::run_prompt` — run a turn and persist it

**Files:**
- Modify: `crates/engine/src/session.rs`

- [ ] **Step 1: Write the failing test**

In `crates/engine/src/session.rs`, add to the `#[cfg(test)] mod tests` block. First extend the imports at the top of the test module (under `use super::*;` / `use otto_persistence::SqliteStore;`):

```rust
    use otto_engine_core::traits::Workspace as _;
    use otto_providers::ScriptedProvider;
    use otto_router::SingleProviderRouter;
    use otto_workspace::LocalWorkspace;
    use std::sync::Arc;
```

Then add the test:

```rust
    #[tokio::test]
    async fn run_prompt_persists_events_and_marks_done() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(&dir).await;

        // Scripted model: planner prompt contains "milestones", coder prompt contains "edits".
        let provider = ScriptedProvider::new("{}")
            .on(
                "edits",
                r#"{"edits": [{"path": "out.txt", "contents": "hi add a greeting"}]}"#,
            )
            .on(
                "milestones",
                r#"{"milestones": [{"description": "write it"}]}"#,
            );
        let router = SingleProviderRouter::new(Arc::new(provider));
        let workspace = LocalWorkspace::new(dir.path());
        let tools_ws: Arc<dyn otto_engine_core::traits::Workspace> =
            Arc::new(LocalWorkspace::new(dir.path()));
        let tools = crate::build_tool_registry(tools_ws, dir.path().to_path_buf());
        let registry = crate::build_default_registry();

        let mut session = Session::create(&store, "add a greeting", &serde_json::json!({}))
            .await
            .unwrap();
        let id = session.id();
        let (events, outcome) = session
            .run_prompt(&registry, &router, &workspace, &tools, "add a greeting")
            .await
            .unwrap();

        assert!(outcome.ok);
        assert_eq!(store.session_status(id).await.unwrap(), SessionStatus::Done);

        // The persisted log equals the returned events, with contiguous seqs from 0.
        let replayed = store.replay_since(id, None).await.unwrap();
        assert_eq!(replayed, events);
        assert!(!events.is_empty());
        for (i, event) in events.iter().enumerate() {
            assert_eq!(event.seq, i as u64);
        }
    }
```

(The `Workspace as _` import is only to bring the `apply_edit`/`read` trait methods into scope if needed; the test above uses `LocalWorkspace` through `run_prompt`, so it is harmless. The `tools_ws` annotation uses the fully-qualified trait to avoid ambiguity.)

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p otto-engine session::tests::run_prompt_persists_events_and_marks_done`
Expected: FAIL to compile — `run_prompt` is not defined on `Session`.

- [ ] **Step 3: Implement `run_prompt`**

In `crates/engine/src/session.rs`, add this method to `impl<'a> Session<'a>` (after `id`):

```rust
    /// Run one orchestrator turn for `goal`, persisting the turn's events (fail-closed: a
    /// store error fails the turn), then recording the turn and updating status to `Done`
    /// (or `Failed`). The per-session `seq` counter continues across calls. Returns the
    /// turn's events and outcome. (≙ `Command::SendPrompt`.)
    pub async fn run_prompt(
        &mut self,
        registry: &AgentRegistry,
        router: &dyn Router,
        workspace: &dyn Workspace,
        tools: &otto_engine_core::tool::ToolRegistry,
        goal: &str,
    ) -> anyhow::Result<(Vec<Event>, TurnOutcome)> {
        let store = self.store;
        let id = self.id;

        let collected: Arc<Mutex<Vec<Event>>> = Arc::new(Mutex::new(Vec::new()));
        let seq = Arc::new(Mutex::new(self.next_seq));
        let sink = {
            let collected = Arc::clone(&collected);
            let seq = Arc::clone(&seq);
            move |kind: EventKind| {
                let mut s = seq.lock().unwrap();
                collected.lock().unwrap().push(Event {
                    seq: *s,
                    session: id,
                    kind,
                });
                *s += 1;
            }
        };

        let orchestrator = Orchestrator {
            registry,
            router,
            workspace,
            tools,
        };
        let outcome = orchestrator.run_turn(id, goal, &sink).await?;
        let events = collected.lock().unwrap().clone();

        // Persist this turn's events. Fail-closed: a store error fails the turn rather than
        // silently dropping events (the durable log is the whole point of the store).
        for event in &events {
            store.append_event(id, event).await?;
        }
        self.next_seq = *seq.lock().unwrap();

        store
            .record_turn(
                id,
                &TurnRecord {
                    turn_index: self.next_turn,
                    goal: goal.to_string(),
                    outcome: serde_json::json!({ "ok": outcome.ok }),
                },
            )
            .await?;
        self.next_turn += 1;

        let status = if outcome.ok {
            SessionStatus::Done
        } else {
            SessionStatus::Failed
        };
        store.set_status(id, status).await?;

        Ok((events, outcome))
    }
```

Add the `ToolRegistry` import to the module's top-level imports (alongside the existing `otto_engine_core` imports):

```rust
use otto_engine_core::tool::ToolRegistry;
```

and change the `run_prompt` `tools` parameter type from `&otto_engine_core::tool::ToolRegistry` to `&ToolRegistry` for readability.

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p otto-engine session::tests::run_prompt_persists_events_and_marks_done`
Expected: PASS. (The unused-import warnings from Task 2 are now gone — all imports are used.)

- [ ] **Step 5: Commit**

```bash
git add crates/engine/src/session.rs
git commit -m "feat(engine): Session::run_prompt runs a turn and persists it"
```

---

### Task 4: Multi-turn seq continuity + `abort`

**Files:**
- Modify: `crates/engine/src/session.rs`

- [ ] **Step 1: Write the failing tests**

In `crates/engine/src/session.rs`, add these two tests to `mod tests`:

```rust
    #[tokio::test]
    async fn second_prompt_continues_seq() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(&dir).await;
        let provider = ScriptedProvider::new("{}")
            .on(
                "edits",
                r#"{"edits": [{"path": "out.txt", "contents": "hi g"}]}"#,
            )
            .on("milestones", r#"{"milestones": [{"description": "x"}]}"#);
        let router = SingleProviderRouter::new(Arc::new(provider));
        let workspace = LocalWorkspace::new(dir.path());
        let tools_ws: Arc<dyn otto_engine_core::traits::Workspace> =
            Arc::new(LocalWorkspace::new(dir.path()));
        let tools = crate::build_tool_registry(tools_ws, dir.path().to_path_buf());
        let registry = crate::build_default_registry();

        let mut session = Session::create(&store, "g", &serde_json::json!({}))
            .await
            .unwrap();
        let id = session.id();
        let (turn1, _) = session
            .run_prompt(&registry, &router, &workspace, &tools, "g")
            .await
            .unwrap();
        let (turn2, _) = session
            .run_prompt(&registry, &router, &workspace, &tools, "g")
            .await
            .unwrap();

        let last1 = turn1.last().unwrap().seq;
        // Turn 2's first event continues right after turn 1's last.
        assert_eq!(turn2.first().unwrap().seq, last1 + 1);

        // The full replayed log is contiguous from 0 and covers both turns.
        let all = store.replay_since(id, None).await.unwrap();
        assert_eq!(all.len(), turn1.len() + turn2.len());
        for (i, event) in all.iter().enumerate() {
            assert_eq!(event.seq, i as u64);
        }

        // Replaying after turn 1's last seq yields exactly turn 2.
        let gap = store.replay_since(id, Some(last1)).await.unwrap();
        assert_eq!(gap, turn2);
    }

    #[tokio::test]
    async fn abort_sets_status_aborted() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(&dir).await;
        let session = Session::create(&store, "g", &serde_json::json!({}))
            .await
            .unwrap();
        session.abort().await.unwrap();
        assert_eq!(
            store.session_status(session.id()).await.unwrap(),
            SessionStatus::Aborted
        );
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p otto-engine session::tests::abort_sets_status_aborted`
Expected: FAIL to compile — `abort` is not defined on `Session`. (`second_prompt_continues_seq` compiles but is the guard for the seq-continuity behavior implemented in Task 3.)

- [ ] **Step 3: Implement `abort`**

In `crates/engine/src/session.rs`, add to `impl<'a> Session<'a>` (after `run_prompt`):

```rust
    /// Mark the session aborted. (≙ `Command::Abort`.)
    pub async fn abort(&self) -> anyhow::Result<()> {
        self.store.set_status(self.id, SessionStatus::Aborted).await
    }
```

- [ ] **Step 4: Run both tests to verify they pass**

Run: `cargo test -p otto-engine session::tests::`
Expected: PASS (all session tests, including `second_prompt_continues_seq` and `abort_sets_status_aborted`).

- [ ] **Step 5: Commit**

```bash
git add crates/engine/src/session.rs
git commit -m "feat(engine): multi-turn seq continuity guard + Session::abort"
```

---

### Task 5: Route `run_goal` through the store; update callers

**Files:**
- Modify: `crates/engine/src/lib.rs`
- Modify: `crates/engine/src/main.rs`
- Modify: `crates/engine/tests/turn.rs`
- Modify: `crates/engine/tests/context.rs`

- [ ] **Step 1: Add `session_config()` and refactor `run_goal` in `lib.rs`**

In `crates/engine/src/lib.rs`, add the `SessionStore` import to the `otto_persistence` use (there is no existing one yet, so add a new line near the other `use` imports):

```rust
use otto_persistence::SessionStore;
```

Add the `session_config` helper (place it just above `run_goal`):

```rust
/// Snapshot the provider-selection environment into JSON for a session's `config` column.
/// Mirrors the env that `build_router` reads (without re-running provider selection), so a
/// stored session records which backends it was configured to use. This lives in the wiring
/// layer (not core) because it reads `OTTO_*` / `ANTHROPIC_API_KEY`.
pub fn session_config() -> serde_json::Value {
    let ollama = std::env::var("OTTO_OLLAMA").as_deref() == Ok("1");
    let anthropic = std::env::var("ANTHROPIC_API_KEY")
        .map(|k| !k.is_empty())
        .unwrap_or(false);
    serde_json::json!({
        "ollama": ollama,
        "anthropic": anthropic,
        "ollama_model": std::env::var("OTTO_OLLAMA_MODEL").ok(),
        "anthropic_model": std::env::var("OTTO_ANTHROPIC_MODEL").ok(),
    })
}
```

Replace the entire `run_goal` function (the `pub async fn run_goal(...) { ... }` block) with:

```rust
/// Run one turn for `goal` against `workspace` using `router`, persisting the session and
/// its events through `store`. Returns the sequenced events emitted and the final outcome.
/// A thin wrapper over `Session`: create the session, then run one prompt.
pub async fn run_goal(
    goal: &str,
    store: &dyn SessionStore,
    router: &dyn Router,
    workspace: &dyn Workspace,
    tools: &ToolRegistry,
) -> anyhow::Result<(Vec<Event>, TurnOutcome)> {
    let registry = build_default_registry();
    let mut session = Session::create(store, goal, &session_config()).await?;
    session
        .run_prompt(&registry, router, workspace, tools, goal)
        .await
}
```

The old `run_goal` imports `EventKind`, `SessionId`, `Arc`, `Mutex` for its hand-rolled sink. After this refactor `run_goal` no longer uses them directly. Leave the existing `use` lines as-is for now; Step 5's `cargo clippy`/`cargo build` will report any that became unused, and you will remove exactly those in Step 5. (`Arc` is still used by `build_tool_registry`; `EventKind`/`SessionId`/`Mutex` likely become unused.)

- [ ] **Step 2: Update `main.rs` to open a store and pass it**

In `crates/engine/src/main.rs`, replace the block that builds the router/workspace/tools and calls `run_goal` (from `let router = build_router();` through the `run_goal(...)` call) with:

```rust
    let router = build_router();
    let workspace = LocalWorkspace::new(root.clone());
    let tools_workspace: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(root.clone()));
    let tools = build_tool_registry(tools_workspace, root);

    // The session store. Defaults to `otto-sessions.db` in the current dir; override with
    // OTTO_DB. Sessions and their event logs accumulate here across runs.
    let db_path = std::env::var("OTTO_DB").unwrap_or_else(|_| "otto-sessions.db".to_string());
    let store = otto_persistence::SqliteStore::open(&db_path).await?;

    let (events, outcome) = run_goal(&goal, &store, router.as_ref(), &workspace, &tools).await?;
```

- [ ] **Step 3: Update the two integration tests**

In `crates/engine/tests/turn.rs`, replace the `run_goal(...)` call:

```rust
    let store = otto_persistence::SqliteStore::open(dir.path().join("sessions.db"))
        .await
        .unwrap();
    let (events, outcome) = run_goal("add a greeting", &store, &router, &workspace, &tools)
        .await
        .unwrap();
```

In `crates/engine/tests/context.rs`, replace the `run_goal(...)` call:

```rust
    let store = otto_persistence::SqliteStore::open(dir.path().join("sessions.db"))
        .await
        .unwrap();
    let (_events, outcome) = run_goal("update the thing function", &store, &router, &workspace, &tools)
        .await
        .unwrap();
```

(Both tests already create `dir` via `tempfile::tempdir()` near the top, so the store path is valid. `otto_persistence` is a normal dependency of `otto-engine`, so integration tests can use it directly.)

- [ ] **Step 4: Run the engine tests to verify they pass**

Run: `cargo test -p otto-engine`
Expected: PASS — the session unit tests, plus `tests/turn.rs` and `tests/context.rs` now driving `run_goal` through a real sqlite store.

- [ ] **Step 5: Remove now-unused imports and verify clean build**

Run: `cargo build -p otto-engine 2>&1 | grep -A2 "unused import"` to see which imports went unused after the `run_goal` refactor. In `crates/engine/src/lib.rs`, remove exactly the now-unused names from the `use otto_protocol::{...}` line and the `use std::sync::{...}` line (expected: `EventKind` and `SessionId` drop from the protocol import leaving `use otto_protocol::{Event, Role};`; `Mutex` drops from `std::sync` leaving `use std::sync::Arc;`). Do not remove anything still in use (`Event`, `Role`, `Arc` remain).

Then run: `cargo build -p otto-engine`
Expected: PASS with no warnings.

- [ ] **Step 6: Commit**

```bash
git add crates/engine/src/lib.rs crates/engine/src/main.rs crates/engine/tests/turn.rs crates/engine/tests/context.rs
git commit -m "feat(engine): run_goal persists through the session store"
```

---

### Task 6: Full workspace verification

**Files:** none (verification only)

- [ ] **Step 1: Format, lint, and test the whole workspace**

Run: `cargo fmt --all && cargo clippy --workspace --all-targets && cargo test --workspace`
Expected: fmt makes no changes (or only trivial ones you then include), clippy is clean across the workspace, and all tests pass — the engine session tests, the two updated integration tests, the persistence crate, and every other crate unchanged.

- [ ] **Step 2: If `cargo fmt` changed anything, commit it**

```bash
git add -A
git commit -m "style(engine): cargo fmt after lifecycle wiring"
```

(If fmt made no changes, skip this commit.)

---

## Done criteria

- `Session` type in `crates/engine/src/session.rs`: `create` (≙ CreateSession), `run_prompt` (≙ SendPrompt), `abort` (≙ Abort), `id`.
- A turn's events are persisted (fail-closed), the turn is recorded, and status moves to `Done`/`Failed`; `abort` sets `Aborted`.
- The per-session `seq` counter is continuous across `run_prompt` calls; `replay_since(id, None)` returns the contiguous full log and `replay_since(id, Some(n))` returns the gap.
- `run_goal` takes a `&dyn SessionStore` and runs through `Session`; `main.rs` opens a `SqliteStore` (`OTTO_DB` or `otto-sessions.db`); both integration tests pass a tempfile store.
- `session_config()` records the provider-selection env in `sessions.config`.
- `cargo test --workspace` green; clippy/fmt clean.

**Next:** Plan C — `SessionState` snapshot/restore derived from the persisted rows (promote-ready; workspace patch-bundle deferred to `RemoteWorkspace`).
