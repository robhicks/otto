# Serve Transport (WebSocket) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship an `otto serve` WebSocket server that speaks the `Command`/`Event` protocol, streams a turn's events live, persists them, and supports `Last-Event-ID` reconnect — built on a transport-agnostic `EngineService` that consolidates Plan B's `Session`.

**Architecture:** Two layers in the `engine` crate. `EngineService` (`service.rs`) owns the store + shared engine deps and runs prompts with a sync→async streaming bridge (spawned orchestrator turn → `mpsc` → per-event persist + `EventSink`). The serve layer (`serve.rs`) is an `axum` WebSocket server mapping WS frames ↔ `Command`/event frames with bearer-token auth and reconnect. `EngineService` replaces `Session`; the CLI and serve both use it.

**Tech Stack:** Rust (edition 2024), `axum` 0.8 (`ws`), `tokio` (`net`/`sync`), `async-trait`, `tokio-tungstenite` (dev), `futures-util` (dev), `otto-persistence`, `otto-protocol`.

**Spec:** `docs/superpowers/specs/2026-06-16-serve-transport-design.md`. Reference it for the deferrals (TLS, concurrent turns, RemoteWorkspace) and the framing rationale.

**Note on networking API details:** Tasks 6–8 use `axum` 0.8 and `tokio-tungstenite` 0.24. If a constructor differs at the resolved patch version (e.g. `Message::Text` taking `String` vs `Utf8Bytes`), adapt the message construction/decoding to the actual API — keep the behavior and the assertions fixed. This is the only place latitude is allowed.

---

### Task 1: Store cursors — `next_seq` / `next_turn`

**Files:**
- Modify: `crates/persistence/src/lib.rs`
- Modify: `crates/persistence/src/sqlite.rs`

- [ ] **Step 1: Add the methods to the `SessionStore` trait**

In `crates/persistence/src/lib.rs`, add to the `SessionStore` trait (after `session_status`):

```rust
    /// The next event seq for `session` (`MAX(seq) + 1`, or 0 if none). Lets a long-lived
    /// or reconnected writer continue the seq sequence without holding an in-memory counter.
    async fn next_seq(&self, session: SessionId) -> anyhow::Result<u64>;

    /// The next turn index for `session` (`MAX(turn_index) + 1`, or 0 if none).
    async fn next_turn(&self, session: SessionId) -> anyhow::Result<u32>;
```

- [ ] **Step 2: Write the failing tests**

In `crates/persistence/src/sqlite.rs`, add to `mod tests` (helpers `temp_store`, `log_event`, `turn` already exist):

```rust
    #[tokio::test]
    async fn cursors_advance_with_events_and_turns() {
        let (store, _dir) = temp_store().await;
        let id = store.create_session("g", &serde_json::json!({})).await.unwrap();
        assert_eq!(store.next_seq(id).await.unwrap(), 0);
        assert_eq!(store.next_turn(id).await.unwrap(), 0);
        store.append_event(id, &log_event(id, 0, "a")).await.unwrap();
        store.append_event(id, &log_event(id, 1, "b")).await.unwrap();
        store.record_turn(id, &turn(0, true)).await.unwrap();
        assert_eq!(store.next_seq(id).await.unwrap(), 2);
        assert_eq!(store.next_turn(id).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn cursors_are_zero_for_unknown_session() {
        let (store, _dir) = temp_store().await;
        let missing = otto_protocol::SessionId::new();
        assert_eq!(store.next_seq(missing).await.unwrap(), 0);
        assert_eq!(store.next_turn(missing).await.unwrap(), 0);
    }
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p otto-persistence sqlite::tests::cursors`
Expected: FAIL to compile — `next_seq`/`next_turn` not implemented for `SqliteStore`.

- [ ] **Step 4: Implement the methods**

In `crates/persistence/src/sqlite.rs`, add to the `impl crate::SessionStore for SqliteStore` block (after `restore`):

```rust
    async fn next_seq(&self, session: otto_protocol::SessionId) -> anyhow::Result<u64> {
        let row: (i64,) =
            sqlx::query_as("SELECT COALESCE(MAX(seq) + 1, 0) FROM events WHERE session_id = ?1")
                .bind(session.0.to_string())
                .fetch_one(&self.pool)
                .await?;
        Ok(row.0 as u64)
    }

    async fn next_turn(&self, session: otto_protocol::SessionId) -> anyhow::Result<u32> {
        let row: (i64,) = sqlx::query_as(
            "SELECT COALESCE(MAX(turn_index) + 1, 0) FROM turns WHERE session_id = ?1",
        )
        .bind(session.0.to_string())
        .fetch_one(&self.pool)
        .await?;
        Ok(row.0 as u32)
    }
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p otto-persistence sqlite::tests::cursors`
Expected: PASS (2 tests).

- [ ] **Step 6: Commit**

```bash
git add crates/persistence/src/lib.rs crates/persistence/src/sqlite.rs
git commit -m "feat(persistence): next_seq/next_turn cursors"
```

---

### Task 2: Engine crate dependencies

**Files:**
- Modify: `crates/engine/Cargo.toml`

- [ ] **Step 1: Add the dependencies**

In `crates/engine/Cargo.toml`, under `[dependencies]`, add:

```toml
async-trait.workspace = true
serde = { workspace = true }
axum = { version = "0.8", features = ["ws"] }
```

Change the existing `tokio` dependency line to add the `net` feature (for `axum::serve`):

```toml
tokio = { workspace = true, features = ["macros", "rt-multi-thread", "fs", "net"] }
```

Under `[dev-dependencies]`, add:

```toml
tokio-tungstenite = "0.24"
futures-util = "0.3"
```

- [ ] **Step 2: Verify it builds**

Run: `cargo build -p otto-engine`
Expected: PASS (downloads axum/tokio-tungstenite; no code uses them yet).

- [ ] **Step 3: Commit**

```bash
git add crates/engine/Cargo.toml Cargo.lock
git commit -m "build(engine): add axum + ws test-client deps"
```

---

### Task 3: `EventSink` + `EngineService` (create/abort/replay/store)

**Files:**
- Create: `crates/engine/src/service.rs`
- Modify: `crates/engine/src/lib.rs`

- [ ] **Step 1: Create the service module with the failing test**

Create `crates/engine/src/service.rs`:

```rust
//! `EngineService`: the transport-agnostic core that runs prompts with live event streaming
//! and persistence. Owns the session store and the shared engine deps; both the CLI and the
//! serve layer drive it. Maps the protocol commands onto operations: `CreateSession` ->
//! `create_session`, `SendPrompt` -> `run_prompt`, `Abort` -> `abort`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use otto_engine_core::tool::ToolRegistry;
use otto_engine_core::traits::Workspace;
use otto_engine_core::{AgentRegistry, Orchestrator, Router, TurnOutcome};
use otto_persistence::{SessionStatus, SessionStore, TurnRecord};
use otto_protocol::{Event, EventKind, SessionId};

/// Receives a turn's events in seq order, each AFTER it is durably persisted. The CLI uses a
/// collecting sink; the serve layer uses one that writes to a WebSocket.
#[async_trait]
pub trait EventSink: Send {
    async fn emit(&mut self, event: &Event) -> anyhow::Result<()>;
}

/// An `EventSink` that gathers events into a `Vec` (used by the CLI / tests).
#[derive(Default)]
pub struct CollectingSink {
    pub events: Vec<Event>,
}

#[async_trait]
impl EventSink for CollectingSink {
    async fn emit(&mut self, event: &Event) -> anyhow::Result<()> {
        self.events.push(event.clone());
        Ok(())
    }
}

/// Runs sessions against a store and a fixed set of engine deps. One turn at a time
/// (`turn_lock`), because the workspace is shared mutable state.
pub struct EngineService {
    store: Arc<dyn SessionStore>,
    registry: Arc<AgentRegistry>,
    router: Arc<dyn Router>,
    workspace: Arc<dyn Workspace>,
    tools: Arc<ToolRegistry>,
    turn_lock: tokio::sync::Mutex<()>,
}

impl EngineService {
    pub fn new(
        store: Arc<dyn SessionStore>,
        registry: Arc<AgentRegistry>,
        router: Arc<dyn Router>,
        workspace: Arc<dyn Workspace>,
        tools: Arc<ToolRegistry>,
    ) -> Self {
        Self {
            store,
            registry,
            router,
            workspace,
            tools,
            turn_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// The session store, for reads the serve layer needs (e.g. replay on reconnect).
    pub fn store(&self) -> &dyn SessionStore {
        &*self.store
    }

    /// Create and persist a new session. (≙ `Command::CreateSession`.)
    pub async fn create_session(
        &self,
        goal: &str,
        config: &serde_json::Value,
    ) -> anyhow::Result<SessionId> {
        self.store.create_session(goal, config).await
    }

    /// Mark a session aborted. (≙ `Command::Abort`.)
    pub async fn abort(&self, session: SessionId) -> anyhow::Result<()> {
        self.store.set_status(session, SessionStatus::Aborted).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use otto_persistence::SqliteStore;
    use otto_providers::ScriptedProvider;
    use otto_router::SingleProviderRouter;
    use otto_workspace::LocalWorkspace;

    fn scripted_router() -> Arc<dyn Router> {
        let provider = ScriptedProvider::new("{}")
            .on(
                "edits",
                r#"{"edits": [{"path": "out.txt", "contents": "hi g"}]}"#,
            )
            .on("milestones", r#"{"milestones": [{"description": "x"}]}"#);
        Arc::new(SingleProviderRouter::new(Arc::new(provider)))
    }

    async fn service_in(dir: &tempfile::TempDir, registry: AgentRegistry) -> EngineService {
        let store: Arc<dyn SessionStore> =
            Arc::new(SqliteStore::open(dir.path().join("s.db")).await.unwrap());
        let workspace: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(dir.path()));
        let tools_ws: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(dir.path()));
        let tools = Arc::new(crate::build_tool_registry(
            tools_ws,
            dir.path().to_path_buf(),
        ));
        EngineService::new(store, Arc::new(registry), scripted_router(), workspace, tools)
    }

    #[tokio::test]
    async fn create_persists_active_session() {
        let dir = tempfile::tempdir().unwrap();
        let service = service_in(&dir, crate::build_default_registry()).await;
        let id = service
            .create_session("do a thing", &serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(
            service.store().session_status(id).await.unwrap(),
            SessionStatus::Active
        );
    }

    #[tokio::test]
    async fn abort_sets_status_aborted() {
        let dir = tempfile::tempdir().unwrap();
        let service = service_in(&dir, crate::build_default_registry()).await;
        let id = service
            .create_session("g", &serde_json::json!({}))
            .await
            .unwrap();
        service.abort(id).await.unwrap();
        assert_eq!(
            service.store().session_status(id).await.unwrap(),
            SessionStatus::Aborted
        );
    }
}
```

Note: `EventKind`, `AtomicU64`, `Ordering`, `Orchestrator`, `TurnOutcome`, `TurnRecord` are imported now but used by `run_prompt` in Task 4 — expected. If an unused-import warning blocks the build before Task 4, it is resolved there; do not delete them.

- [ ] **Step 2: Register the module in `lib.rs`**

In `crates/engine/src/lib.rs`, add near the top (alongside `mod session;`):

```rust
mod service;

pub use service::{CollectingSink, EngineService, EventSink};
```

- [ ] **Step 3: Run the tests to verify they pass**

Run: `cargo test -p otto-engine service::`
Expected: PASS (2 tests).

- [ ] **Step 4: Commit**

```bash
git add crates/engine/src/service.rs crates/engine/src/lib.rs
git commit -m "feat(engine): EventSink + EngineService (create/abort)"
```

---

### Task 4: `EngineService::run_prompt` (streaming)

**Files:**
- Modify: `crates/engine/src/service.rs`

- [ ] **Step 1: Write the failing tests**

In `crates/engine/src/service.rs`, add to `mod tests`:

```rust
    #[tokio::test]
    async fn run_prompt_streams_persists_and_marks_done() {
        let dir = tempfile::tempdir().unwrap();
        let service = service_in(&dir, crate::build_default_registry()).await;
        let id = service
            .create_session("add a greeting", &serde_json::json!({}))
            .await
            .unwrap();
        let mut sink = CollectingSink::default();
        let outcome = service.run_prompt(id, "add a greeting", &mut sink).await.unwrap();

        assert!(outcome.ok);
        assert_eq!(
            service.store().session_status(id).await.unwrap(),
            SessionStatus::Done
        );
        // The streamed events equal the persisted log, with contiguous seqs from 0.
        let replayed = service.store().replay_since(id, None).await.unwrap();
        assert_eq!(replayed, sink.events);
        assert!(!sink.events.is_empty());
        for (i, event) in sink.events.iter().enumerate() {
            assert_eq!(event.seq, i as u64);
        }
    }

    #[tokio::test]
    async fn second_prompt_continues_seq() {
        let dir = tempfile::tempdir().unwrap();
        let service = service_in(&dir, crate::build_default_registry()).await;
        let id = service.create_session("g", &serde_json::json!({})).await.unwrap();

        let mut s1 = CollectingSink::default();
        service.run_prompt(id, "g", &mut s1).await.unwrap();
        let mut s2 = CollectingSink::default();
        service.run_prompt(id, "g", &mut s2).await.unwrap();

        let last1 = s1.events.last().unwrap().seq;
        assert_eq!(s2.events.first().unwrap().seq, last1 + 1);

        let all = service.store().replay_since(id, None).await.unwrap();
        assert_eq!(all.len(), s1.events.len() + s2.events.len());
        for (i, event) in all.iter().enumerate() {
            assert_eq!(event.seq, i as u64);
        }
    }

    #[tokio::test]
    async fn orchestrator_error_marks_session_failed() {
        let dir = tempfile::tempdir().unwrap();
        // Empty registry: the orchestrator can't find the Planner, so run_turn errors.
        let service = service_in(&dir, AgentRegistry::new()).await;
        let id = service.create_session("g", &serde_json::json!({})).await.unwrap();
        let mut sink = CollectingSink::default();
        let result = service.run_prompt(id, "g", &mut sink).await;
        assert!(result.is_err());
        assert_eq!(
            service.store().session_status(id).await.unwrap(),
            SessionStatus::Failed
        );
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p otto-engine service::tests::run_prompt_streams_persists_and_marks_done`
Expected: FAIL to compile — `run_prompt` is not defined on `EngineService`.

- [ ] **Step 3: Implement `run_prompt`**

In `crates/engine/src/service.rs`, add this method to `impl EngineService` (after `abort`):

```rust
    /// Run one orchestrator turn for `goal`, streaming each event to `sink` as it is emitted
    /// (after persisting it, fail-closed), then recording the turn and updating status
    /// (`Done`/`Failed`; an orchestrator error also sets `Failed`). The seq sequence
    /// continues from the store. Serialized: one turn at a time. (≙ `Command::SendPrompt`.)
    pub async fn run_prompt(
        &self,
        session: SessionId,
        goal: &str,
        sink: &mut dyn EventSink,
    ) -> anyhow::Result<TurnOutcome> {
        let _guard = self.turn_lock.lock().await;

        let start_seq = self.store.next_seq(session).await?;
        let turn_index = self.store.next_turn(session).await?;

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Event>();

        // Spawn the turn. Its sync sink assigns seqs and pushes events into the channel; the
        // orchestrator borrows the shared deps via the Arc clones moved into the task.
        let handle = {
            let registry = Arc::clone(&self.registry);
            let router = Arc::clone(&self.router);
            let workspace = Arc::clone(&self.workspace);
            let tools = Arc::clone(&self.tools);
            let goal = goal.to_string();
            let counter = Arc::new(AtomicU64::new(start_seq));
            tokio::spawn(async move {
                let sink_fn = move |kind: EventKind| {
                    let seq = counter.fetch_add(1, Ordering::SeqCst);
                    let _ = tx.send(Event {
                        seq,
                        session,
                        kind,
                    });
                };
                let orchestrator = Orchestrator {
                    registry: &registry,
                    router: &*router,
                    workspace: &*workspace,
                    tools: &tools,
                };
                orchestrator.run_turn(session, &goal, &sink_fn).await
            })
        };

        // Drain live: persist each event (fail-closed) then forward to the sink, in order.
        let mut stream_err: Option<anyhow::Error> = None;
        while let Some(event) = rx.recv().await {
            if let Err(e) = self.store.append_event(session, &event).await {
                stream_err = Some(e);
                break;
            }
            if let Err(e) = sink.emit(&event).await {
                stream_err = Some(e);
                break;
            }
        }
        drop(rx); // any further sends from the (still finishing) turn task are dropped

        let turn_result = handle.await?; // JoinError propagates

        if let Some(e) = stream_err {
            let _ = self.store.set_status(session, SessionStatus::Failed).await;
            return Err(e);
        }
        let outcome = match turn_result {
            Ok(outcome) => outcome,
            Err(e) => {
                let _ = self.store.set_status(session, SessionStatus::Failed).await;
                return Err(e);
            }
        };

        self.store
            .record_turn(
                session,
                &TurnRecord {
                    turn_index,
                    goal: goal.to_string(),
                    outcome: serde_json::json!({ "ok": outcome.ok }),
                },
            )
            .await?;
        let status = if outcome.ok {
            SessionStatus::Done
        } else {
            SessionStatus::Failed
        };
        self.store.set_status(session, status).await?;

        Ok(outcome)
    }
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p otto-engine service::`
Expected: PASS (all service tests). All Task-3 imports are now used.

- [ ] **Step 5: Commit**

```bash
git add crates/engine/src/service.rs
git commit -m "feat(engine): EngineService::run_prompt streams + persists a turn"
```

---

### Task 5: Remove `Session`; route `run_goal` + callers through `EngineService`

**Files:**
- Delete: `crates/engine/src/session.rs`
- Modify: `crates/engine/src/lib.rs`
- Modify: `crates/engine/src/main.rs`
- Modify: `crates/engine/tests/turn.rs`
- Modify: `crates/engine/tests/context.rs`

- [ ] **Step 1: Remove the `session` module and refactor `run_goal`**

In `crates/engine/src/lib.rs`:

Delete the `mod session;` line and the `pub use session::Session;` line.

Replace the entire `run_goal` function with this Arc-taking version that runs through `EngineService`:

```rust
/// Run one turn for `goal` through an `EngineService` backed by `store`, returning the
/// sequenced events and the outcome. A thin wrapper: build the service, create a session,
/// run one prompt with a collecting sink.
pub async fn run_goal(
    goal: &str,
    store: Arc<dyn SessionStore>,
    router: Arc<dyn Router>,
    workspace: Arc<dyn Workspace>,
    tools: Arc<ToolRegistry>,
) -> anyhow::Result<(Vec<Event>, TurnOutcome)> {
    let service = EngineService::new(
        store,
        Arc::new(build_default_registry()),
        router,
        workspace,
        tools,
    );
    let session = service.create_session(goal, &session_config()).await?;
    let mut sink = CollectingSink::default();
    let outcome = service.run_prompt(session, goal, &mut sink).await?;
    Ok((sink.events, outcome))
}
```

Then delete the file `crates/engine/src/session.rs`.

After this, fix imports in `lib.rs`: the `run_goal` rewrite changes which names are used. Ensure these are imported (add any missing, and after Step 5's build remove any that became unused): `std::sync::Arc`; `otto_engine_core::{Router, TurnOutcome}`; `otto_engine_core::tool::ToolRegistry`; `otto_engine_core::traits::Workspace`; `otto_persistence::SessionStore`; `otto_protocol::Event`; and the `pub use service::{CollectingSink, EngineService, EventSink};` from Task 3 (the `CollectingSink`/`EngineService` names are used here).

- [ ] **Step 2: Update `main.rs` to pass owned `Arc` deps**

In `crates/engine/src/main.rs`, replace the block from `let router = build_router();` through the `run_goal(...)` call with:

```rust
    let router: Arc<dyn otto_engine_core::Router> = Arc::from(build_router());
    let orch_workspace: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(root.clone()));
    let tools_workspace: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(root.clone()));
    let tools = Arc::new(build_tool_registry(tools_workspace, root.clone()));

    // The session store. Defaults to `otto-sessions.db` in the current dir; override with
    // OTTO_DB. Sessions and their event logs accumulate here across runs.
    let db_path = std::env::var("OTTO_DB").unwrap_or_else(|_| "otto-sessions.db".to_string());
    let store: Arc<dyn otto_persistence::SessionStore> =
        Arc::new(otto_persistence::SqliteStore::open(&db_path).await?);

    let (events, outcome) = run_goal(&goal, store, router, orch_workspace, tools).await?;
```

- [ ] **Step 3: Update `tests/turn.rs`**

In `crates/engine/tests/turn.rs`, change the `workspace`/`tools`/store/`run_goal` section so the deps are `Arc`s. Replace from `let router = SingleProviderRouter::new(Arc::new(provider));` through the `run_goal(...)` call with:

```rust
    let router: Arc<dyn otto_engine_core::Router> =
        Arc::new(SingleProviderRouter::new(Arc::new(provider)));
    let workspace: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(dir.path()));
    let tools_workspace: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(dir.path()));
    let tools = Arc::new(build_tool_registry(tools_workspace, dir.path().to_path_buf()));
    let store: Arc<dyn otto_persistence::SessionStore> =
        Arc::new(otto_persistence::SqliteStore::open(dir.path().join("sessions.db")).await.unwrap());

    let (events, outcome) = run_goal("add a greeting", store, router, workspace.clone(), tools)
        .await
        .unwrap();
```

The later assertion reads the written file via `workspace.read(...)`. Since `workspace` is now `Arc<dyn Workspace>` and `WorkspaceRead` is in scope, `workspace.read(Path::new("otto_output.txt"))` still works (`workspace.clone()` was passed to `run_goal`, the original `Arc` is kept for the read). Leave that assertion as-is.

- [ ] **Step 4: Update `tests/context.rs`**

In `crates/engine/tests/context.rs`, replace from `let router = SingleProviderRouter::new(Arc::new(provider));` through the `run_goal(...)` call with:

```rust
    let router: Arc<dyn otto_engine_core::Router> =
        Arc::new(SingleProviderRouter::new(Arc::new(provider)));
    let workspace: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(dir.path()));
    let tools_workspace: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(dir.path()));
    let tools = Arc::new(build_tool_registry(tools_workspace, dir.path().to_path_buf()));
    let store: Arc<dyn otto_persistence::SessionStore> =
        Arc::new(otto_persistence::SqliteStore::open(dir.path().join("sessions.db")).await.unwrap());

    let (_events, outcome) =
        run_goal("update the thing function", store, router, workspace.clone(), tools)
            .await
            .unwrap();
```

The later `workspace.read(Path::new("result.txt"))` assertion works the same way — keep it.

- [ ] **Step 5: Build, remove any unused imports, and run engine tests**

Run: `cargo build -p otto-engine --all-targets 2>&1 | grep -A2 "unused"` and remove any reported now-unused imports in `lib.rs`/`main.rs` (do not remove names still in use).

Then run: `cargo test -p otto-engine`
Expected: PASS — `service::` tests, plus `tests/turn.rs` and `tests/context.rs` driving `run_goal` through `EngineService`. `session.rs` is gone; no references remain.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "refactor(engine): replace Session with EngineService; run_goal uses it"
```

---

### Task 6: The axum WebSocket serve module

**Files:**
- Create: `crates/engine/src/serve.rs`
- Modify: `crates/engine/src/lib.rs`

- [ ] **Step 1: Create the serve module**

Create `crates/engine/src/serve.rs`. (Adapt `Message::Text` construction/decoding to the resolved `axum` 0.8 API if it differs — keep behavior fixed.)

```rust
//! WebSocket transport for the engine. Maps WS frames to `Command`/event frames over an
//! `EngineService`: bearer-token auth on upgrade, a `Ready { session }` frame on connect,
//! optional `Last-Event-ID` replay (`?last_seq=`), then live streamed events per `SendPrompt`.
//! Binds loopback; TLS and concurrent sessions are out of scope (see the design spec).

use std::sync::Arc;

use axum::Router as AxumRouter;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use otto_protocol::{Command, Event, SessionId};
use serde::{Deserialize, Serialize};

use crate::service::{EngineService, EventSink};

/// Outbound WS frame. Reuses the core `Event`; `Ready`/`Error` are transport framing.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerMessage {
    Ready { session: SessionId },
    Event { event: Event },
    Error { message: String },
}

#[derive(Deserialize, Default)]
struct ConnectParams {
    session: Option<String>,
    last_seq: Option<u64>,
}

/// Shared server state: the engine service and the required bearer token.
struct ServeState {
    service: EngineService,
    token: String,
}

/// Build the axum app. Exposed for tests so they can serve it on an ephemeral port.
pub fn app(service: EngineService, token: String) -> AxumRouter {
    let state = Arc::new(ServeState { service, token });
    AxumRouter::new().route("/ws", get(ws_handler)).with_state(state)
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    headers: HeaderMap,
    Query(params): Query<ConnectParams>,
    State(state): State<Arc<ServeState>>,
) -> Response {
    let presented = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    if presented != Some(state.token.as_str()) {
        return (StatusCode::UNAUTHORIZED, "missing or invalid bearer token").into_response();
    }
    ws.on_upgrade(move |socket| handle_socket(socket, params, state))
}

/// Send one `ServerMessage` as a JSON text frame.
async fn send_msg(socket: &mut WebSocket, msg: &ServerMessage) -> anyhow::Result<()> {
    let json = serde_json::to_string(msg)?;
    socket.send(Message::Text(json.into())).await?;
    Ok(())
}

/// A sink that writes each event to the socket as a `ServerMessage::Event` frame.
struct WsSink<'a> {
    socket: &'a mut WebSocket,
}

#[async_trait::async_trait]
impl EventSink for WsSink<'_> {
    async fn emit(&mut self, event: &Event) -> anyhow::Result<()> {
        send_msg(self.socket, &ServerMessage::Event { event: event.clone() }).await
    }
}

async fn handle_socket(mut socket: WebSocket, params: ConnectParams, state: Arc<ServeState>) {
    // Resolve the session: reuse `?session=<uuid>` or mint a new one.
    let session = match resolve_session(&params, &state).await {
        Ok(s) => s,
        Err(e) => {
            let _ = send_msg(&mut socket, &ServerMessage::Error { message: e.to_string() }).await;
            return;
        }
    };

    if send_msg(&mut socket, &ServerMessage::Ready { session }).await.is_err() {
        return;
    }

    // Reconnect: replay the gap after `last_seq`.
    if let Some(after) = params.last_seq {
        match state.service.store().replay_since(session, Some(after)).await {
            Ok(events) => {
                for event in events {
                    if send_msg(&mut socket, &ServerMessage::Event { event }).await.is_err() {
                        return;
                    }
                }
            }
            Err(e) => {
                let _ = send_msg(&mut socket, &ServerMessage::Error { message: e.to_string() }).await;
                return;
            }
        }
    }

    // Command loop. One command at a time; a turn runs to completion before the next.
    while let Some(Ok(msg)) = socket.recv().await {
        let text = match msg {
            Message::Text(t) => t,
            Message::Close(_) => break,
            _ => continue, // ignore binary/ping/pong
        };
        let command: Command = match serde_json::from_str(text.as_str()) {
            Ok(c) => c,
            Err(e) => {
                let _ = send_msg(&mut socket, &ServerMessage::Error { message: format!("bad command: {e}") }).await;
                continue;
            }
        };
        match command {
            Command::SendPrompt { text, .. } => {
                let mut sink = WsSink { socket: &mut socket };
                if let Err(e) = state.service.run_prompt(session, &text, &mut sink).await {
                    let _ = send_msg(&mut socket, &ServerMessage::Error { message: e.to_string() }).await;
                }
            }
            Command::Abort { .. } => {
                let _ = state.service.abort(session).await;
                break;
            }
            Command::CreateSession => {
                // The session is already established on connect; nothing to do.
            }
        }
    }
}

async fn resolve_session(
    params: &ConnectParams,
    state: &ServeState,
) -> anyhow::Result<SessionId> {
    match &params.session {
        Some(s) => {
            let uuid = uuid::Uuid::parse_str(s)?;
            Ok(SessionId(uuid))
        }
        None => state.service.create_session("", &serde_json::json!({})).await,
    }
}
```

Note: `resolve_session` parses a UUID, so the engine crate needs `uuid`. Add `uuid.workspace = true` to `crates/engine/Cargo.toml` `[dependencies]` in this task (it is a workspace dep). If you prefer not to add it, instead reuse the fact that `SessionId(pub Uuid)` — but parsing the string still requires `uuid::Uuid::parse_str`, so add the dep.

- [ ] **Step 2: Register the module and the dep**

In `crates/engine/src/lib.rs`, add (alongside the other `mod` lines):

```rust
mod serve;

pub use serve::app as serve_app;
```

In `crates/engine/Cargo.toml` `[dependencies]`, add:

```toml
uuid.workspace = true
```

- [ ] **Step 3: Verify it builds**

Run: `cargo build -p otto-engine`
Expected: PASS. (No test yet; the integration test lands in Task 8.)

- [ ] **Step 4: Commit**

```bash
git add crates/engine/src/serve.rs crates/engine/src/lib.rs crates/engine/Cargo.toml Cargo.lock
git commit -m "feat(engine): axum WebSocket serve module (auth, reconnect, streaming)"
```

---

### Task 7: `otto serve` subcommand

**Files:**
- Modify: `crates/engine/src/main.rs`

- [ ] **Step 1: Restructure `main` to dispatch `run` / `serve`**

Replace the entire body of `main` in `crates/engine/src/main.rs` with a subcommand dispatch. Keep the existing `run` behavior; add `serve`. The full new `main.rs`:

```rust
//! `otto run "<goal>" [--root <path>]` — run a single turn and print the event stream.
//! `otto serve [--root <path>] [--port <p>]` — serve the engine over WebSocket (needs OTTO_TOKEN).

use std::path::PathBuf;
use std::sync::Arc;

use otto_engine::{build_router, build_tool_registry, run_goal, serve_app};
use otto_engine_core::traits::Workspace;
use otto_workspace::LocalWorkspace;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let command = args.next().unwrap_or_default();
    let rest: Vec<String> = args.collect();

    match command.as_str() {
        "run" => cmd_run(rest).await,
        "serve" => cmd_serve(rest).await,
        _ => {
            eprintln!("usage:\n  otto run \"<goal>\" [--root <path>]\n  otto serve [--root <path>] [--port <p>]");
            std::process::exit(2);
        }
    }
}

/// Parse `--root <path>` from args, defaulting to ".". Returns (root, remaining positional).
fn parse_root(args: &[String]) -> (PathBuf, Vec<String>) {
    let mut root = PathBuf::from(".");
    let mut positional = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--root" {
            if let Some(p) = it.next() {
                root = PathBuf::from(p);
            } else {
                eprintln!("error: --root requires a path");
                std::process::exit(2);
            }
        } else {
            positional.push(a.clone());
        }
    }
    (root, positional)
}

fn open_db_path() -> String {
    std::env::var("OTTO_DB").unwrap_or_else(|_| "otto-sessions.db".to_string())
}

async fn cmd_run(args: Vec<String>) -> anyhow::Result<()> {
    let (root, positional) = parse_root(&args);
    let goal = positional.into_iter().next().unwrap_or_else(|| {
        eprintln!("error: missing goal");
        std::process::exit(2);
    });

    let router: Arc<dyn otto_engine_core::Router> = Arc::from(build_router());
    let orch_workspace: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(root.clone()));
    let tools_workspace: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(root.clone()));
    let tools = Arc::new(build_tool_registry(tools_workspace, root.clone()));
    let store: Arc<dyn otto_persistence::SessionStore> =
        Arc::new(otto_persistence::SqliteStore::open(&open_db_path()).await?);

    let (events, outcome) = run_goal(&goal, store, router, orch_workspace, tools).await?;
    for event in &events {
        println!("[{:>3}] {:?}", event.seq, event.kind);
    }
    println!("turn ok = {}", outcome.ok);
    if !outcome.ok {
        std::process::exit(1);
    }
    Ok(())
}

async fn cmd_serve(args: Vec<String>) -> anyhow::Result<()> {
    let (root, positional) = parse_root(&args);
    let mut port: u16 = std::env::var("OTTO_PORT").ok().and_then(|s| s.parse().ok()).unwrap_or(7878);
    let mut it = positional.iter();
    while let Some(a) = it.next() {
        if a == "--port" {
            match it.next().and_then(|s| s.parse().ok()) {
                Some(p) => port = p,
                None => {
                    eprintln!("error: --port requires a number");
                    std::process::exit(2);
                }
            }
        }
    }

    // Auth is mandatory and fail-closed: refuse to start without a token.
    let token = match std::env::var("OTTO_TOKEN") {
        Ok(t) if !t.is_empty() => t,
        _ => {
            eprintln!("error: OTTO_TOKEN must be set to run `otto serve`");
            std::process::exit(2);
        }
    };

    let router: Arc<dyn otto_engine_core::Router> = Arc::from(build_router());
    let orch_workspace: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(root.clone()));
    let tools_workspace: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(root.clone()));
    let tools = Arc::new(build_tool_registry(tools_workspace, root.clone()));
    let store: Arc<dyn otto_persistence::SessionStore> =
        Arc::new(otto_persistence::SqliteStore::open(&open_db_path()).await?);
    let registry = Arc::new(otto_engine::build_default_registry());

    let service = otto_engine::EngineService::new(store, registry, router, orch_workspace, tools);
    let app = serve_app(service, token);

    let addr = format!("127.0.0.1:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    eprintln!("otto serve listening on ws://{addr}/ws");
    axum::serve(listener, app).await?;
    Ok(())
}
```

Note: this references `otto_engine::build_default_registry` and `otto_engine::EngineService` — confirm `build_default_registry` is `pub` in `lib.rs` (it is) and that `EngineService` is re-exported (it is, from Task 3). `otto_persistence` is a dependency of `otto-engine` and usable in the binary.

- [ ] **Step 2: Verify it builds**

Run: `cargo build -p otto-engine`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add crates/engine/src/main.rs
git commit -m "feat(engine): otto serve subcommand"
```

---

### Task 8: Loopback WebSocket integration test

**Files:**
- Create: `crates/engine/tests/serve.rs`

- [ ] **Step 1: Write the integration test**

Create `crates/engine/tests/serve.rs`. (Adapt the `tokio-tungstenite` 0.24 message construction/decoding to the resolved API if it differs — keep the assertions fixed.)

```rust
//! End-to-end: the axum WebSocket server streams a turn's events to a connected client,
//! supports Last-Event-ID reconnect, and rejects unauthenticated connections. Runs on a
//! loopback ephemeral port — no external network.

use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use otto_engine::{EngineService, build_default_registry, build_tool_registry, serve_app};
use otto_engine_core::traits::Workspace;
use otto_providers::ScriptedProvider;
use otto_router::SingleProviderRouter;
use otto_workspace::LocalWorkspace;
use serde_json::Value;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

const TOKEN: &str = "test-token";

/// Start the serve app on 127.0.0.1:0 and return the bound port. Keeps the tempdir alive.
async fn start_server() -> (u16, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let provider = ScriptedProvider::new("{}")
        .on(
            "edits",
            r#"{"edits": [{"path": "out.txt", "contents": "hi add a greeting"}]}"#,
        )
        .on("milestones", r#"{"milestones": [{"description": "x"}]}"#);
    let router: Arc<dyn otto_engine_core::Router> =
        Arc::new(SingleProviderRouter::new(Arc::new(provider)));
    let workspace: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(dir.path()));
    let tools_ws: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(dir.path()));
    let tools = Arc::new(build_tool_registry(tools_ws, dir.path().to_path_buf()));
    let store: Arc<dyn otto_persistence::SessionStore> =
        Arc::new(otto_persistence::SqliteStore::open(dir.path().join("s.db")).await.unwrap());
    let service = EngineService::new(store, Arc::new(build_default_registry()), router, workspace, tools);

    let app = serve_app(service, TOKEN.to_string());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (port, dir)
}

fn authed_request(port: u16, query: &str) -> tokio_tungstenite::tungstenite::handshake::client::Request {
    let url = format!("ws://127.0.0.1:{port}/ws{query}");
    let mut req = url.into_client_request().unwrap();
    req.headers_mut()
        .insert("Authorization", format!("Bearer {TOKEN}").parse().unwrap());
    req
}

#[tokio::test]
async fn streams_a_turn_then_reconnects_with_replay() {
    let (port, _dir) = start_server().await;

    // First connection: new session, send a prompt, collect streamed frames.
    let (mut ws, _) = tokio_tungstenite::connect_async(authed_request(port, ""))
        .await
        .expect("connect");

    // First frame is Ready { session }.
    let ready: Value = next_json(&mut ws).await;
    assert_eq!(ready["type"], "ready");
    let session = ready["session"].as_str().unwrap().to_string();

    // Send a prompt for that session.
    let cmd = serde_json::json!({ "SendPrompt": { "session": session, "text": "add a greeting" } });
    ws.send(Message::Text(serde_json::to_string(&cmd).unwrap().into()))
        .await
        .unwrap();

    // Collect event frames until TurnComplete.
    let mut seqs = Vec::new();
    let mut saw_turn_complete = false;
    while let Some(frame) = next_json_opt(&mut ws).await {
        if frame["type"] == "event" {
            let kind = &frame["event"]["kind"];
            seqs.push(frame["event"]["seq"].as_u64().unwrap());
            if kind.get("TurnComplete").is_some() {
                saw_turn_complete = true;
                break;
            }
        }
    }
    assert!(saw_turn_complete, "expected a TurnComplete event");
    assert_eq!(seqs.first(), Some(&0), "events start at seq 0");
    let last_seq = *seqs.last().unwrap();
    drop(ws);

    // Reconnect with last_seq = 0: expect the gap (events with seq > 0) replayed.
    let (mut ws2, _) =
        tokio_tungstenite::connect_async(authed_request(port, &format!("?session={session}&last_seq=0")))
            .await
            .expect("reconnect");
    let ready2: Value = next_json(&mut ws2).await;
    assert_eq!(ready2["type"], "ready");
    assert_eq!(ready2["session"].as_str().unwrap(), session);

    let mut replayed = Vec::new();
    // The gap is finite; read until we've seen the last seq, then stop.
    while let Some(frame) = next_json_opt(&mut ws2).await {
        if frame["type"] == "event" {
            let seq = frame["event"]["seq"].as_u64().unwrap();
            replayed.push(seq);
            if seq == last_seq {
                break;
            }
        }
    }
    assert!(replayed.iter().all(|&s| s > 0), "replay gap excludes seq 0");
    assert_eq!(replayed.last(), Some(&last_seq));
}

#[tokio::test]
async fn rejects_missing_token() {
    let (port, _dir) = start_server().await;
    let url = format!("ws://127.0.0.1:{port}/ws");
    // No Authorization header → upgrade rejected (401), connect_async errors.
    let result = tokio_tungstenite::connect_async(url).await;
    assert!(result.is_err(), "unauthenticated connection must be rejected");
}

/// Receive the next text frame as JSON (panics on close/non-text or stream end).
async fn next_json(ws: &mut (impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin)) -> Value {
    next_json_opt(ws).await.expect("expected a frame")
}

/// Receive the next text frame as JSON, or None if the stream ended / a non-text frame arrived.
async fn next_json_opt(ws: &mut (impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin)) -> Option<Value> {
    loop {
        match ws.next().await {
            Some(Ok(Message::Text(t))) => return Some(serde_json::from_str(t.as_str()).unwrap()),
            Some(Ok(Message::Close(_))) | None => return None,
            Some(Ok(_)) => continue, // skip ping/pong/binary
            Some(Err(_)) => return None,
        }
    }
}
```

- [ ] **Step 2: Run the integration test**

Run: `cargo test -p otto-engine --test serve`
Expected: PASS (2 tests). If the `tokio-tungstenite` `Request` type path or `Message::Text` argument differs at the resolved version, adjust those lines (e.g. `Message::text(...)`, or `t.as_str()` vs `&t`) — keep the assertions unchanged.

- [ ] **Step 3: Commit**

```bash
git add crates/engine/tests/serve.rs
git commit -m "test(engine): loopback WebSocket serve integration test"
```

---

### Task 9: Full workspace verification

**Files:** none (verification only)

- [ ] **Step 1: Format, lint, and test the whole workspace**

Run: `cargo fmt --all && cargo clippy --workspace --all-targets && cargo test --workspace`
Expected: fmt clean (or trivial changes you then include), clippy clean across the workspace, all tests green — persistence cursors, the `EngineService` tests, both refactored integration tests, the serve integration test, and every other crate unchanged.

- [ ] **Step 2: If `cargo fmt` changed anything, commit it**

```bash
git add -A
git commit -m "style: cargo fmt after serve transport"
```

(If fmt made no changes, skip this commit.)

---

## Done criteria

- `SessionStore` has `next_seq`/`next_turn`; `SqliteStore` implements them (0 for unknown sessions).
- `EventSink` trait + `CollectingSink`; `EngineService` (`new`/`create_session`/`abort`/`run_prompt`/`store`) streams a turn's events live (spawn → mpsc → persist → sink), serialized one turn at a time, fail-closed, error→`Failed`.
- `Session` is removed; `run_goal` and `main`'s `run` path go through `EngineService`; both integration tests pass.
- `otto serve` starts an axum WS server (loopback, `--port`/`OTTO_PORT`), refuses to start without `OTTO_TOKEN`, authenticates each upgrade, sends `Ready`, replays the `last_seq` gap, and streams turn events; `Abort` aborts.
- Loopback integration test proves stream + reconnect-replay + auth rejection.
- `cargo test --workspace` green; clippy/fmt clean.

**Arc note:** This completes the first network transport. Remaining distribution-axis work (separate arcs): TLS/WSS + `RemoteTarget`/`RemoteWorkspace` for true remote deployment and promote-to-remote (where the deferred `workspace_root` lands), and a browser-friendly auth path.
