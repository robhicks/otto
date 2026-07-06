# `Command::RunAgent` on `otto serve` Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let a client connected to `otto serve` dispatch a discovered `.claude/agents/*.md` custom agent by name (`Command::RunAgent`), mirroring what `otto run --agent <name> "<goal>"` already does on the CLI — closing the last "`--agent` subpath on serve" thread.

**Architecture:** A new `Command::RunAgent { session, name, prompt }` protocol variant is resolved entirely inside `EngineService` via a new `run_agent_with_controls` method. Unlike `RunCommand` (which preprocesses a goal string and runs it through the ordinary `Orchestrator::run_turn`), custom-agent dispatch is a single non-interruptible `TaskTool::call` → `MarkdownAgent::run` request/response — there is no orchestrator turn to draw events from. `run_agent_with_controls` synthesizes the turn's event stream from the *existing* `EventKind` vocabulary (`AgentStarted`, `Log`, `AgentFinished`, `TurnComplete` — no new wire variant), using the same persist-then-stream discipline (and `turn_lock`/seq/turn bookkeeping) `run_prompt_with_controls` already uses, and reusing the server's already-composed tool registry (`self.tools`) so a dispatched agent's `tools:` allowlist is enforced against the fully-composed permissions/hooks/skills/plugin-MCP registry, not an empty one. `serve.rs`'s WebSocket handler dispatches to it directly (no `run_turn_loop` — there is nothing to pause or approve mid-dispatch).

**Tech Stack:** Rust workspace (`otto-protocol`, `otto-engine`, `otto-extensions` crates), tokio, axum WebSockets, `anyhow`.

**Design spec:** `docs/superpowers/specs/2026-07-05-serve-run-agent-design.md` — read it first for the full rationale, in particular the event-synthesis decision and why this needed its own design distinct from `RunCommand`.

---

### Task 1: Protocol — `Command::RunAgent`

**Files:**
- Modify: `crates/protocol/src/lib.rs:51-55` (the `Command` enum, right after `RunCommand`)
- Modify: `crates/protocol/src/lib.rs` (test module, right after `run_command_command_round_trips`, currently ending around line 311)

- [ ] **Step 1: Add the new variant**

In `crates/protocol/src/lib.rs`, inside `pub enum Command { ... }`, add a new variant right after `RunCommand` (before `Abort`):

```rust
    /// Dispatch a discovered `.claude/agents/*.md` custom agent by name as a single,
    /// non-interruptible request/response (no orchestrator turn): compose its system prompt with
    /// `prompt` and run it through `TaskTool`/`MarkdownAgent`. Emits the existing
    /// `AgentStarted`/`Log`/`AgentFinished`/`TurnComplete` `EventKind`s — no new wire variant.
    /// Unknown `name` surfaces as `ServerMessage::Error` — no turn starts, no `seq` is consumed.
    RunAgent {
        session: SessionId,
        name: String,
        prompt: String,
    },
```

Leave every other variant untouched (the enum still ends with `Abort`, `ApproveDiff`, `Pause`, `Resume`, `PromoteToRemote`, `DemoteToLocal`).

- [ ] **Step 2: Write the round-trip test**

Add this test to the `#[cfg(test)] mod tests` block, right after `run_command_command_round_trips` (around line 311):

```rust
    #[test]
    fn run_agent_command_round_trips() {
        let cmd = Command::RunAgent {
            session: SessionId::new(),
            name: "reviewer".to_string(),
            prompt: "look at auth.rs".to_string(),
        };
        let json = serde_json::to_string(&cmd).unwrap();
        let back: Command = serde_json::from_str(&json).unwrap();
        assert_eq!(cmd, back);
        // External tagging matches the rest of Command (e.g. {"RunAgent":{...}}).
        assert!(json.contains("\"RunAgent\""));
    }
```

- [ ] **Step 3: Run the tests**

Run: `cargo test -p otto-protocol`
Expected: all tests pass, including the new `run_agent_command_round_trips`.

- [ ] **Step 4: Commit**

```bash
git add crates/protocol/src/lib.rs
git commit -m "feat(protocol): add Command::RunAgent"
```

---

### Task 2: `EngineService::run_agent_with_controls`

**Files:**
- Modify: `crates/engine/src/service.rs:1-20` (imports)
- Modify: `crates/engine/src/service.rs:322` (insert the new method(s) right after `run_command_with_controls`, before `validate_workspace_edits`)
- Modify: `crates/engine/src/service.rs` (test module — new tests)

- [ ] **Step 1: Extend imports**

In `crates/engine/src/service.rs`, replace the import block:

```rust
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use otto_engine_core::tool::{
    Approver, Decision, DenyApprover, NeverPause, PauseController, ToolRegistry,
};
use otto_engine_core::traits::Workspace;
use otto_engine_core::types::Edit;
use otto_engine_core::{AgentRegistry, Orchestrator, Retriever, Router, TokenMeter, TurnOutcome};
use otto_extensions::{Extensions, expand_args, resolve_injections};
use otto_persistence::{SessionStatus, SessionStore, TurnRecord};
use otto_protocol::{Event, EventKind, SessionId, WorkspaceRequest, WorkspaceResponse};
use otto_router::MeteringRouter;
use serde_json::json;
```

with:

```rust
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use otto_engine_core::tool::{
    Approver, Decision, DenyApprover, NeverPause, PauseController, ToolRegistry,
};
use otto_engine_core::traits::{Workspace, WorkspaceRead};
use otto_engine_core::types::Edit;
use otto_engine_core::{AgentRegistry, Orchestrator, Retriever, Router, TokenMeter, TurnOutcome};
use otto_extensions::{Extensions, MarkdownAgent, TaskTool, expand_args, resolve_injections};
use otto_persistence::{SessionStatus, SessionStore, TurnRecord};
use otto_protocol::{Event, EventKind, Role, SessionId, WorkspaceRequest, WorkspaceResponse};
use otto_router::MeteringRouter;
use serde_json::json;
```

- [ ] **Step 2: Add `run_agent_with_controls` and its two helpers**

Insert this right after `run_command_with_controls` ends (after the closing `}` at line 322, before the `/// Validate every workspace file...` doc comment on `validate_workspace_edits`):

```rust

    /// Look up `name` in the discovered custom agents (set via `with_extensions`), dispatch it as
    /// a single, non-interruptible `TaskTool`/`MarkdownAgent` call (no `Orchestrator::run_turn`),
    /// and synthesize the turn's event stream from the existing `EventKind` vocabulary:
    /// `AgentStarted`, `Log` (the agent's full response text), `AgentFinished`, `TurnComplete`.
    /// (≙ `Command::RunAgent`.) Errors before any event is emitted — unknown `name`, or no
    /// extensions attached via `with_extensions` at all — so no `seq` is consumed and the session
    /// is untouched. A dispatch failure (e.g. a provider error) still emits
    /// `AgentFinished`/`TurnComplete { ok: false }` and marks the session `Failed`, matching how
    /// an orchestrator failure is reported for `SendPrompt`/`RunCommand`.
    pub async fn run_agent_with_controls(
        &self,
        session: SessionId,
        name: &str,
        prompt: &str,
        sink: &mut dyn EventSink,
    ) -> anyhow::Result<TurnOutcome> {
        let extensions = self.extensions.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "no custom agent named '{name}': this server was not configured with any extensions"
            )
        })?;
        if !extensions.agents.iter().any(|a| a.name == name) {
            anyhow::bail!(
                "no custom agent named '{name}' in ~/.claude/agents/ or the project .claude/agents/"
            );
        }

        // Register EVERY discovered custom agent (not just `name`), matching the CLI's
        // `run_custom_agent_in` byte-for-byte — cheap, since the defs are already in memory from
        // startup discovery.
        let mut registry = AgentRegistry::new();
        let mut allowlists: HashMap<String, Option<Vec<String>>> = HashMap::new();
        let mut model_override: Option<String> = None;
        for def in &extensions.agents {
            if def.name == name {
                model_override = def.model.clone();
            }
            allowlists.insert(def.name.clone(), def.tools.clone());
            registry.register(
                Role::Custom(def.name.clone()),
                Arc::new(MarkdownAgent::new(def.clone())),
            );
        }

        // Always freshly built (never falls back to self.router), matching
        // run_command_with_controls's model-pinning convention exactly.
        let router: Arc<dyn Router> =
            Arc::from(crate::build_router_with_model(model_override.as_deref()));
        // `Workspace: WorkspaceRead` — this is a supertrait upcast, not a second workspace.
        let read_ws: Arc<dyn WorkspaceRead> = self.workspace.clone();
        let task = TaskTool::new(
            router,
            read_ws,
            Arc::new(registry),
            Arc::clone(&self.tools),
            allowlists,
        );

        let _guard = self.turn_lock.lock().await;
        let start_seq = self.store.next_seq(session).await?;
        let turn_index = self.store.next_turn(session).await?;
        let counter = AtomicU64::new(start_seq);
        let role = Role::Custom(name.to_string());

        // From here, seq/turn_index are reserved: any failure past this point still marks the
        // session Failed, matching run_prompt_with_controls's contract for an in-flight turn.
        let outcome = self
            .run_agent_dispatch(session, &counter, &role, name, prompt, &task, sink)
            .await;

        match &outcome {
            Ok(turn_outcome) => {
                self.store
                    .record_turn(
                        session,
                        &TurnRecord {
                            turn_index,
                            goal: prompt.to_string(),
                            outcome: serde_json::json!({ "ok": turn_outcome.ok }),
                        },
                    )
                    .await?;
                let status = if turn_outcome.ok {
                    SessionStatus::Done
                } else {
                    SessionStatus::Failed
                };
                self.store.set_status(session, status).await?;
            }
            Err(_) => {
                let _ = self.store.set_status(session, SessionStatus::Failed).await;
            }
        }

        outcome
    }

    /// Emit the fixed `AgentStarted`/`Log`/`AgentFinished`/`TurnComplete` sequence for a single
    /// `TaskTool` dispatch, persisting each event before streaming it (fail-closed). A `task.call`
    /// failure still emits `AgentFinished`/`TurnComplete { ok: false }` — the error is threaded
    /// into the returned `Err`, never swallowed.
    async fn run_agent_dispatch(
        &self,
        session: SessionId,
        counter: &AtomicU64,
        role: &Role,
        name: &str,
        prompt: &str,
        task: &TaskTool,
        sink: &mut dyn EventSink,
    ) -> anyhow::Result<TurnOutcome> {
        self.emit_and_persist(
            session,
            counter,
            sink,
            EventKind::AgentStarted { role: role.clone() },
        )
        .await?;

        let dispatch = task
            .call(serde_json::json!({ "agent": name, "prompt": prompt }))
            .await
            .and_then(|out| {
                out.get("text")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .ok_or_else(|| anyhow::anyhow!("task dispatch returned no `text` field"))
            });

        if let Ok(text) = &dispatch {
            self.emit_and_persist(
                session,
                counter,
                sink,
                EventKind::Log {
                    message: text.clone(),
                },
            )
            .await?;
        }

        self.emit_and_persist(
            session,
            counter,
            sink,
            EventKind::AgentFinished { role: role.clone() },
        )
        .await?;

        let ok = dispatch.is_ok();
        self.emit_and_persist(session, counter, sink, EventKind::TurnComplete { ok })
            .await?;

        match dispatch {
            Ok(_) => Ok(TurnOutcome { ok: true }),
            Err(e) => Err(e),
        }
    }

    /// Assign the next seq, persist the event (fail-closed), then stream it to `sink`. Shared by
    /// `run_agent_dispatch`'s fixed event sequence.
    async fn emit_and_persist(
        &self,
        session: SessionId,
        counter: &AtomicU64,
        sink: &mut dyn EventSink,
        kind: EventKind,
    ) -> anyhow::Result<()> {
        let seq = counter.fetch_add(1, Ordering::SeqCst);
        let event = Event { seq, session, kind };
        self.store.append_event(session, &event).await?;
        sink.emit(&event).await?;
        Ok(())
    }
```

- [ ] **Step 3: Build**

Run: `cargo build -p otto-engine`
Expected: compiles cleanly (no test yet exercises the new method, but it must type-check).

- [ ] **Step 4: Commit**

```bash
git add crates/engine/src/service.rs
git commit -m "feat(engine): add EngineService::run_agent_with_controls"
```

---

### Task 3: `EngineService` unit tests for `run_agent_with_controls`

**Files:**
- Modify: `crates/engine/src/service.rs` (test module, after the `run_command_with_controls_*` tests, i.e. after the test ending around line 1152 and before `create_persists_active_session`)

- [ ] **Step 1: Add an `agent_def` test helper and the tests**

Add this helper right after the existing `command_def` helper (around line 976, after its closing `}`):

```rust
    fn agent_def(
        name: &str,
        system_prompt: &str,
        tools: Option<Vec<String>>,
        model: Option<String>,
    ) -> otto_extensions::CustomAgentDef {
        otto_extensions::CustomAgentDef {
            name: name.to_string(),
            description: "d".to_string(),
            tools,
            model,
            system_prompt: system_prompt.to_string(),
        }
    }
```

Then add these tests, right after `run_command_with_controls_injection_failure_errors_without_starting_a_turn` (around line 1152, before `create_persists_active_session`):

```rust
    #[tokio::test]
    async fn run_agent_with_controls_unknown_name_errors_without_starting_a_turn() {
        let dir = tempfile::tempdir().unwrap();
        let service = service_in(&dir, crate::build_default_registry())
            .await
            .with_extensions(Arc::new(Extensions::default()));
        let id = service
            .create_session("g", &serde_json::json!({}))
            .await
            .unwrap();

        let mut sink = CollectingSink::default();
        let err = service
            .run_agent_with_controls(id, "ghost", "do it", &mut sink)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no custom agent named 'ghost'"));

        let replayed = service.store().replay_since(id, None).await.unwrap();
        assert!(replayed.is_empty(), "no turn should have started");
    }

    #[tokio::test]
    async fn run_agent_with_controls_no_extensions_configured_errors() {
        let dir = tempfile::tempdir().unwrap();
        // No `.with_extensions(...)` call at all — `self.extensions` stays `None`.
        let service = service_in(&dir, crate::build_default_registry()).await;
        let id = service
            .create_session("g", &serde_json::json!({}))
            .await
            .unwrap();

        let mut sink = CollectingSink::default();
        let err = service
            .run_agent_with_controls(id, "anything", "do it", &mut sink)
            .await
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("not configured with any extensions")
        );
    }

    #[tokio::test]
    async fn run_agent_with_controls_dispatches_and_streams_events() {
        let dir = tempfile::tempdir().unwrap();
        let def = agent_def("reviewer", "SYSTEM-PROMPT", Some(vec!["fs.read".to_string()]), None);
        let extensions = Extensions {
            agents: vec![def],
            ..Default::default()
        };
        let service = service_in(&dir, crate::build_default_registry())
            .await
            .with_extensions(Arc::new(extensions));
        let id = service
            .create_session("g", &serde_json::json!({}))
            .await
            .unwrap();

        let mut sink = CollectingSink::default();
        let outcome = service
            .run_agent_with_controls(id, "reviewer", "look at auth.rs", &mut sink)
            .await
            .unwrap();
        assert!(outcome.ok);

        // Fixed event sequence: AgentStarted, Log, AgentFinished, TurnComplete.
        assert_eq!(sink.events.len(), 4);
        assert!(matches!(
            sink.events[0].kind,
            EventKind::AgentStarted {
                role: Role::Custom(ref n)
            } if n == "reviewer"
        ));
        match &sink.events[1].kind {
            EventKind::Log { message } => {
                // The offline LocalProvider's deterministic completion echoes its prompt, so the
                // composed system prompt + task prompt both surface in the logged text.
                assert!(message.contains("SYSTEM-PROMPT"));
                assert!(message.contains("look at auth.rs"));
            }
            other => panic!("expected Log, got {other:?}"),
        }
        assert!(matches!(
            sink.events[2].kind,
            EventKind::AgentFinished {
                role: Role::Custom(ref n)
            } if n == "reviewer"
        ));
        assert!(matches!(
            sink.events[3].kind,
            EventKind::TurnComplete { ok: true }
        ));

        // Persisted log matches what was streamed, with contiguous seqs from 0.
        let replayed = service.store().replay_since(id, None).await.unwrap();
        assert_eq!(replayed, sink.events);

        assert_eq!(
            service.store().session_status(id).await.unwrap(),
            SessionStatus::Done
        );
    }

    #[tokio::test]
    async fn run_agent_with_controls_registers_every_discovered_agent() {
        // Two agents discovered; dispatching the second must still work — proving the
        // "register every discovered def" loop doesn't only wire up the first/target one.
        let dir = tempfile::tempdir().unwrap();
        let extensions = Extensions {
            agents: vec![
                agent_def("first", "FIRST-PROMPT", None, None),
                agent_def("second", "SECOND-PROMPT", None, None),
            ],
            ..Default::default()
        };
        let service = service_in(&dir, crate::build_default_registry())
            .await
            .with_extensions(Arc::new(extensions));
        let id = service
            .create_session("g", &serde_json::json!({}))
            .await
            .unwrap();

        let mut sink = CollectingSink::default();
        let outcome = service
            .run_agent_with_controls(id, "second", "ping", &mut sink)
            .await
            .unwrap();
        assert!(outcome.ok);
        match &sink.events[1].kind {
            EventKind::Log { message } => assert!(message.contains("SECOND-PROMPT")),
            other => panic!("expected Log, got {other:?}"),
        }
    }
```

- [ ] **Step 2: Run the tests**

Run: `cargo test -p otto-engine service::`
Expected: all `run_agent_with_controls_*` tests pass, along with the pre-existing `service::tests` suite (regression).

- [ ] **Step 3: Commit**

```bash
git add crates/engine/src/service.rs
git commit -m "test(engine): cover EngineService::run_agent_with_controls"
```

---

### Task 4: Wire `Command::RunAgent` into `serve.rs`

**Files:**
- Modify: `crates/engine/src/serve.rs:638-655` (the `Command::RunCommand` arm — add the new arm right after it, before the closing `}` of the `match command` block)

- [ ] **Step 1: Add the `RunAgent` arm**

In `crates/engine/src/serve.rs`, inside `handle_socket`'s `match command { ... }`, add this arm right after the `Command::RunCommand { .. } => { ... }` arm (before the final closing `}` of the match):

```rust
            Command::RunAgent { name, prompt, .. } => {
                // No `run_turn_loop`: a single TaskTool dispatch has no fs.write gate check to
                // approve and no multi-step turn to pause between steps of (see the design spec).
                let outcome = {
                    let mut sink = WsSink {
                        writer: &mut writer,
                    };
                    state
                        .service
                        .run_agent_with_controls(session, &name, &prompt, &mut sink)
                        .await
                }; // `sink` dropped here → `writer` is free again

                if report_turn_outcome(TurnLoopOutcome::Finished(outcome.err()), &mut writer).await
                {
                    break 'outer;
                }
            }
```

- [ ] **Step 2: Build**

Run: `cargo build -p otto-engine`
Expected: compiles cleanly.

- [ ] **Step 3: Commit**

```bash
git add crates/engine/src/serve.rs
git commit -m "feat(engine): dispatch Command::RunAgent on otto serve"
```

---

### Task 5: `serve.rs` integration tests

**Files:**
- Modify: `crates/engine/tests/serve.rs` (add a new `start_server_with_agent` fixture near `start_server_with_command`, and two new `#[tokio::test]`s near the `run_command_*` tests)

- [ ] **Step 1: Add the fixture**

In `crates/engine/tests/serve.rs`, add this right after `start_server_with_command` ends (after its closing `}`, around line 122):

```rust
/// Start the serve app with one discovered custom agent: `reviewer`, system prompt
/// `"SYSTEM-PROMPT"`. Uses the deterministic offline router (no ScriptedProvider needed — the
/// dispatched `MarkdownAgent` goes through `build_router_with_model`, not the service's own
/// router).
async fn start_server_with_agent() -> (u16, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let provider = ScriptedProvider::new("{}")
        .on(
            "edits",
            r#"{"edits": [{"path": "out.txt", "contents": "should not land"}]}"#,
        )
        .on("milestones", r#"{"milestones": [{"description": "x"}]}"#);
    let router: Arc<dyn otto_engine_core::Router> =
        Arc::new(SingleProviderRouter::new(Arc::new(provider)));
    let workspace: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(dir.path()));
    let tools_ws: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(dir.path()));
    let tools = Arc::new(build_tool_registry(tools_ws, dir.path().to_path_buf()));
    let store: Arc<dyn otto_persistence::SessionStore> = Arc::new(
        otto_persistence::SqliteStore::open(dir.path().join("s.db"))
            .await
            .unwrap(),
    );
    let extensions = Extensions {
        agents: vec![otto_extensions::CustomAgentDef {
            name: "reviewer".to_string(),
            description: "d".to_string(),
            tools: None,
            model: None,
            system_prompt: "SYSTEM-PROMPT".to_string(),
        }],
        ..Default::default()
    };
    let service = EngineService::new(
        store,
        Arc::new(build_default_registry()),
        router,
        workspace,
        tools,
    )
    .with_extensions(Arc::new(extensions));

    let app = serve_app(service, TOKEN.to_string(), test_capabilities(), None, false);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        serve_run(listener, app, None).await.unwrap();
    });
    (port, dir)
}
```

Add `CustomAgentDef` to the existing `use otto_extensions::{CustomCommandDef, Extensions};` import at the top of the file (change it to `use otto_extensions::{CustomAgentDef, CustomCommandDef, Extensions};`) — actually the fixture above already fully-qualifies it as `otto_extensions::CustomAgentDef`, so no import change is required; leave the existing `use` line untouched.

- [ ] **Step 2: Add the two tests**

Add these right after `run_command_unknown_name_reports_error_and_keeps_connection_open` ends (around line 630, before the reconnect/replay tests that follow):

```rust
#[tokio::test]
async fn run_agent_dispatches_and_reports_turn_complete() {
    let (port, dir) = start_server_with_agent().await;
    let (mut ws, _) = tokio_tungstenite::connect_async(authed_request(port, ""))
        .await
        .expect("connect");
    let ready: Value = next_json(&mut ws).await;
    let session = ready["session"].as_str().unwrap().to_string();

    let cmd = serde_json::json!({
        "RunAgent": { "session": session, "name": "reviewer", "prompt": "look at auth.rs" }
    });
    ws.send(Message::Text(serde_json::to_string(&cmd).unwrap()))
        .await
        .unwrap();

    let mut saw_started = false;
    let mut saw_log_with_prompt = false;
    let mut completed = false;
    while let Some(frame) = next_json_opt(&mut ws).await {
        if frame["type"] != "event" {
            continue;
        }
        let kind = &frame["event"]["kind"];
        if kind.get("AgentStarted").is_some() {
            saw_started = true;
        }
        if let Some(log) = kind.get("Log") {
            if log["message"].as_str().unwrap_or("").contains("look at auth.rs") {
                saw_log_with_prompt = true;
            }
        }
        if kind.get("TurnComplete").is_some() {
            completed = true;
            break;
        }
    }
    assert!(saw_started, "expected an AgentStarted event");
    assert!(saw_log_with_prompt, "expected a Log event carrying the dispatched prompt");
    assert!(completed, "expected the RunAgent call to complete");

    // A custom-agent dispatch never touches the workspace via fs.write — no orchestrator edit
    // was ever proposed for this call.
    assert!(!dir.path().join("out.txt").exists());
}

#[tokio::test]
async fn run_agent_unknown_name_reports_error_and_keeps_connection_open() {
    let (port, _dir) = start_server_with_agent().await;
    let (mut ws, _) = tokio_tungstenite::connect_async(authed_request(port, ""))
        .await
        .expect("connect");
    let ready: Value = next_json(&mut ws).await;
    let session = ready["session"].as_str().unwrap().to_string();

    let cmd = serde_json::json!({
        "RunAgent": { "session": session.clone(), "name": "ghost", "prompt": "x" }
    });
    ws.send(Message::Text(serde_json::to_string(&cmd).unwrap()))
        .await
        .unwrap();

    let frame = next_json(&mut ws).await;
    assert_eq!(frame["type"], "error");
    assert!(
        frame["message"]
            .as_str()
            .unwrap()
            .contains("no custom agent named 'ghost'"),
        "got: {frame}"
    );

    // The connection is still usable afterward — SendPrompt still completes a turn.
    let cmd2 =
        serde_json::json!({ "SendPrompt": { "session": session, "text": "add a greeting" } });
    ws.send(Message::Text(serde_json::to_string(&cmd2).unwrap()))
        .await
        .unwrap();
    let mut completed = false;
    while let Some(frame) = next_json_opt(&mut ws).await {
        if frame["type"] == "event" && frame["event"]["kind"].get("TurnComplete").is_some() {
            completed = true;
            break;
        }
    }
    assert!(completed, "connection must survive an unknown RunAgent");
}
```

- [ ] **Step 3: Run the tests**

Run: `cargo test -p otto-engine --test serve`
Expected: all tests pass, including the two new `run_agent_*` tests, alongside the pre-existing `run_command_*` and other serve tests (regression).

- [ ] **Step 4: Commit**

```bash
git add crates/engine/tests/serve.rs
git commit -m "test(engine): serve-path Command::RunAgent integration coverage"
```

---

### Task 6: Full workspace verification and CLAUDE.md update

**Files:**
- Modify: `/home/robhicks/dev/otto-next/CLAUDE.md` (the `extensions` row of the crate table)

- [ ] **Step 1: Run the full test suite**

Run: `cargo test --workspace`
Expected: all tests pass (offline/deterministic — no network or API keys needed).

- [ ] **Step 2: Format and lint**

Run: `cargo fmt --all && cargo clippy --workspace --all-targets`
Expected: `fmt` makes no changes (or only whitespace on the lines just touched); `clippy` reports no new warnings.

- [ ] **Step 3: Update CLAUDE.md**

In `/home/robhicks/dev/otto-next/CLAUDE.md`, find the sentence in the `extensions` row of the crate table that currently reads:

> "`--agent` on serve remains the only deferred serve-path thread (a separate plan: it bypasses the orchestrator turn machinery entirely, via `TaskTool`/`MarkdownAgent`, and needs its own event-synthesis design)."

Replace it with:

> Slice 13 ships **`Command::RunAgent` on `otto serve`**: `EngineService` gains `run_agent_with_controls`, which looks up a discovered custom agent by name, dispatches it as a single `TaskTool`/`MarkdownAgent` call (no `Orchestrator::run_turn` — there is no turn machinery for a one-shot completion), and synthesizes its event stream from the existing `EventKind` vocabulary (`AgentStarted`/`Log`/`AgentFinished`/`TurnComplete` — no new wire variant), reusing the server's already-composed tool registry so a dispatched agent's `tools:` allowlist is enforced against the fully-composed permissions/hooks/skills/plugin-MCP registry rather than an empty one (the same serve-is-strictly-more-correct-than-CLI asymmetry `RunCommand` already has). `serve.rs` dispatches it directly, without `run_turn_loop` — a one-shot completion has no `fs.write` gate check to approve and no multi-step turn to pause between steps of. This closes the last deferred serve-path thread from the extensions rollout.

- [ ] **Step 4: Commit**

```bash
git add CLAUDE.md
git commit -m "docs: update CLAUDE.md for Command::RunAgent on serve"
```
