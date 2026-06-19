//! `EngineService`: the transport-agnostic core that runs prompts with live event streaming
//! and persistence. Owns the session store and the shared engine deps; both the CLI and the
//! serve layer drive it. Maps the protocol commands onto operations: `CreateSession` ->
//! `create_session`, `SendPrompt` -> `run_prompt`, `Abort` -> `abort`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use otto_engine_core::tool::{
    Approver, Decision, DenyApprover, NeverPause, PauseController, ToolRegistry,
};
use otto_engine_core::traits::Workspace;
use otto_engine_core::types::Edit;
use otto_engine_core::{AgentRegistry, Orchestrator, Router, TokenMeter, TurnOutcome};
use otto_persistence::{SessionStatus, SessionStore, TurnRecord};
use otto_protocol::{Event, EventKind, SessionId, WorkspaceRequest, WorkspaceResponse};
use otto_router::MeteringRouter;
use serde_json::json;

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

/// The per-turn control handles the serve layer can inject: how `Ask` edits are approved, and
/// how the turn is paused. `Default` is the headless/CLI posture (deny approvals, never pause).
pub struct TurnControls {
    pub approver: Arc<dyn Approver>,
    pub pauser: Arc<dyn PauseController>,
}

impl Default for TurnControls {
    fn default() -> Self {
        Self {
            approver: Arc::new(DenyApprover),
            pauser: Arc::new(NeverPause),
        }
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

    /// Run one turn with the headless defaults (deny approvals, never pause). (≙ `SendPrompt`.)
    pub async fn run_prompt(
        &self,
        session: SessionId,
        goal: &str,
        sink: &mut dyn EventSink,
    ) -> anyhow::Result<TurnOutcome> {
        self.run_prompt_with_controls(session, goal, sink, TurnControls::default())
            .await
    }

    /// Back-compat wrapper: run with `approver` and no pause. (Used by serve until it migrates
    /// to `run_prompt_with_controls`.)
    pub async fn run_prompt_with_approver(
        &self,
        session: SessionId,
        goal: &str,
        sink: &mut dyn EventSink,
        approver: Arc<dyn Approver>,
    ) -> anyhow::Result<TurnOutcome> {
        self.run_prompt_with_controls(
            session,
            goal,
            sink,
            TurnControls {
                approver,
                pauser: Arc::new(NeverPause),
            },
        )
        .await
    }

    /// Run one orchestrator turn for `goal`, streaming each event to `sink` after persisting it
    /// (fail-closed), recording the turn, and updating status. `controls` supply the approver
    /// and pause controller. The seq sequence continues from the store. One turn at a time.
    pub async fn run_prompt_with_controls(
        &self,
        session: SessionId,
        goal: &str,
        sink: &mut dyn EventSink,
        controls: TurnControls,
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
            let approver = Arc::clone(&controls.approver);
            let pauser = Arc::clone(&controls.pauser);
            tokio::spawn(async move {
                // Per-turn meter; the metering router tallies usage as completions pass through.
                let meter = Arc::new(TokenMeter::default());
                let metering_router = MeteringRouter::new(router, Arc::clone(&meter));
                let sink_fn = move |kind: EventKind| {
                    let seq = counter.fetch_add(1, Ordering::SeqCst);
                    let _ = tx.send(Event { seq, session, kind });
                };
                let next_id = || uuid::Uuid::new_v4();
                let orchestrator = Orchestrator {
                    registry: &registry,
                    router: &metering_router,
                    workspace: &*workspace,
                    tools: &tools,
                    approver: &*approver,
                    next_id: &next_id,
                    meter: &meter,
                    pauser: &*pauser,
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

    /// Handle one unary workspace RPC against this service's workspace. `read` and
    /// `apply_edit` are routed through the permission gate (Allow-only), so the
    /// network-exposed primitive cannot read/write sensitive paths even though it bypasses
    /// the orchestrator. `list` and `snapshot` are ALSO gate-filtered: any path the read
    /// gate denies is omitted from the result, so a sensitive non-dotfile (e.g. `id_rsa`)
    /// cannot appear in a listing or snapshot even if the workspace walk includes it.
    pub async fn workspace_rpc(&self, req: WorkspaceRequest) -> WorkspaceResponse {
        match req {
            WorkspaceRequest::Read { path } => {
                if self
                    .tools
                    .check("fs.read", &json!({ "path": path.to_string_lossy() }))
                    != Decision::Allow
                {
                    return WorkspaceResponse::Error {
                        message: format!("read denied by permission gate: {}", path.display()),
                    };
                }
                match self.workspace.read(&path).await {
                    Ok(bytes) => WorkspaceResponse::Read { bytes },
                    Err(e) => WorkspaceResponse::Error {
                        message: e.to_string(),
                    },
                }
            }
            WorkspaceRequest::List { glob } => match self.workspace.list(&glob).await {
                Ok(mut paths) => {
                    paths.retain(|p| {
                        self.tools
                            .check("fs.read", &json!({ "path": p.to_string_lossy() }))
                            == Decision::Allow
                    });
                    WorkspaceResponse::List { paths }
                }
                Err(e) => WorkspaceResponse::Error {
                    message: e.to_string(),
                },
            },
            WorkspaceRequest::ApplyEdit { path, contents } => {
                // A direct workspace write is the client's own explicit action (editor save /
                // promote), not an agent edit, so it needs no interactive approval: reject only on
                // an outright `Deny`. This matters under `--approve-edits`, where the gate upgrades
                // benign `fs.write` to `Ask` — treating `Ask` as denial here would silently break
                // every direct write while approval mode is on. The inviolable sensitive-path floor
                // still returns `Deny` and stays blocked.
                if self
                    .tools
                    .check("fs.write", &json!({ "path": path.to_string_lossy() }))
                    == Decision::Deny
                {
                    return WorkspaceResponse::Error {
                        message: format!("write denied by permission gate: {}", path.display()),
                    };
                }
                let edit = Edit {
                    path,
                    new_contents: contents,
                };
                match self.workspace.apply_edit(&edit).await {
                    Ok(bytes_written) => WorkspaceResponse::ApplyEdit { bytes_written },
                    Err(e) => WorkspaceResponse::Error {
                        message: e.to_string(),
                    },
                }
            }
            WorkspaceRequest::Snapshot => match self.workspace.snapshot().await {
                Ok(snap) => {
                    let files: Vec<_> = snap
                        .files
                        .into_iter()
                        .filter(|(p, _)| {
                            self.tools
                                .check("fs.read", &json!({ "path": p.to_string_lossy() }))
                                == Decision::Allow
                        })
                        .collect();
                    WorkspaceResponse::Snapshot { files }
                }
                Err(e) => WorkspaceResponse::Error {
                    message: e.to_string(),
                },
            },
        }
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

    fn metered_router() -> Arc<dyn Router> {
        let provider = ScriptedProvider::new("{}")
            .on(
                "edits",
                r#"{"edits": [{"path": "out.txt", "contents": "hi g"}]}"#,
            )
            .on("milestones", r#"{"milestones": [{"description": "x"}]}"#)
            .with_usage(10, 20);
        Arc::new(SingleProviderRouter::new(Arc::new(provider)))
    }

    #[tokio::test]
    async fn run_prompt_streams_token_cost_meter_with_usage() {
        let dir = tempfile::tempdir().unwrap();
        let store: Arc<dyn SessionStore> =
            Arc::new(SqliteStore::open(dir.path().join("s.db")).await.unwrap());
        let workspace: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(dir.path()));
        let tools_ws: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(dir.path()));
        let tools = Arc::new(crate::build_tool_registry(
            tools_ws,
            dir.path().to_path_buf(),
        ));
        let service = EngineService::new(
            store,
            Arc::new(crate::build_default_registry()),
            metered_router(),
            workspace,
            tools,
        );
        let id = service
            .create_session("add a greeting", &serde_json::json!({}))
            .await
            .unwrap();
        let mut sink = CollectingSink::default();
        service
            .run_prompt(id, "add a greeting", &mut sink)
            .await
            .unwrap();

        let meters: Vec<_> = sink
            .events
            .iter()
            .filter_map(|e| match e.kind {
                EventKind::TokenCostMeter {
                    input_tokens,
                    output_tokens,
                } => Some((input_tokens, output_tokens)),
                _ => None,
            })
            .collect();
        assert!(
            !meters.is_empty(),
            "expected at least one TokenCostMeter event"
        );
        for w in meters.windows(2) {
            assert!(w[1].0 >= w[0].0 && w[1].1 >= w[0].1);
        }
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
        EngineService::new(
            store,
            Arc::new(registry),
            scripted_router(),
            workspace,
            tools,
        )
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

    #[tokio::test]
    async fn run_prompt_streams_persists_and_marks_done() {
        let dir = tempfile::tempdir().unwrap();
        let service = service_in(&dir, crate::build_default_registry()).await;
        let id = service
            .create_session("add a greeting", &serde_json::json!({}))
            .await
            .unwrap();
        let mut sink = CollectingSink::default();
        let outcome = service
            .run_prompt(id, "add a greeting", &mut sink)
            .await
            .unwrap();

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
        let id = service
            .create_session("g", &serde_json::json!({}))
            .await
            .unwrap();

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
    async fn workspace_rpc_write_read_list_snapshot() {
        use otto_protocol::{WorkspaceRequest, WorkspaceResponse};
        let dir = tempfile::tempdir().unwrap();
        let service = service_in(&dir, crate::build_default_registry()).await;

        // Write
        match service
            .workspace_rpc(WorkspaceRequest::ApplyEdit {
                path: std::path::PathBuf::from("a.txt"),
                contents: "hi".to_string(),
            })
            .await
        {
            WorkspaceResponse::ApplyEdit { bytes_written } => assert_eq!(bytes_written, 2),
            other => panic!("unexpected: {other:?}"),
        }
        // Read it back
        match service
            .workspace_rpc(WorkspaceRequest::Read {
                path: std::path::PathBuf::from("a.txt"),
            })
            .await
        {
            WorkspaceResponse::Read { bytes } => assert_eq!(bytes, b"hi".to_vec()),
            other => panic!("unexpected: {other:?}"),
        }
        // List + Snapshot return Ok variants
        assert!(matches!(
            service
                .workspace_rpc(WorkspaceRequest::List {
                    glob: "**".to_string()
                })
                .await,
            WorkspaceResponse::List { .. }
        ));
        assert!(matches!(
            service.workspace_rpc(WorkspaceRequest::Snapshot).await,
            WorkspaceResponse::Snapshot { .. }
        ));
    }

    #[tokio::test]
    async fn workspace_rpc_gates_sensitive_write_and_read() {
        use otto_protocol::{WorkspaceRequest, WorkspaceResponse};
        let dir = tempfile::tempdir().unwrap();
        let service = service_in(&dir, crate::build_default_registry()).await;

        // Writing a sensitive path is denied by the gate floor (nothing written).
        assert!(matches!(
            service
                .workspace_rpc(WorkspaceRequest::ApplyEdit {
                    path: std::path::PathBuf::from(".env"),
                    contents: "SECRET=x".to_string(),
                })
                .await,
            WorkspaceResponse::Error { .. }
        ));
        // Reading a sensitive path is denied too.
        assert!(matches!(
            service
                .workspace_rpc(WorkspaceRequest::Read {
                    path: std::path::PathBuf::from(".env"),
                })
                .await,
            WorkspaceResponse::Error { .. }
        ));
    }

    #[tokio::test]
    async fn workspace_rpc_apply_edit_ok_under_approval_mode() {
        // Under `--approve-edits` the gate upgrades benign `fs.write` Allow->Ask. A direct
        // workspace RPC write is the client's explicit action and must still succeed (Ask is not
        // a denial here); only the sensitive floor (Deny) blocks.
        use otto_protocol::{WorkspaceRequest, WorkspaceResponse};
        let dir = tempfile::tempdir().unwrap();
        let store: Arc<dyn SessionStore> =
            Arc::new(SqliteStore::open(dir.path().join("s.db")).await.unwrap());
        let workspace: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(dir.path()));
        let tools_ws: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(dir.path()));
        let tools = Arc::new(crate::build_tool_registry_approving(
            tools_ws,
            dir.path().to_path_buf(),
        ));
        let service = EngineService::new(
            store,
            Arc::new(crate::build_default_registry()),
            scripted_router(),
            workspace,
            tools,
        );

        match service
            .workspace_rpc(WorkspaceRequest::ApplyEdit {
                path: std::path::PathBuf::from("a.txt"),
                contents: "hi".to_string(),
            })
            .await
        {
            WorkspaceResponse::ApplyEdit { bytes_written } => assert_eq!(bytes_written, 2),
            other => panic!("benign write should succeed under approval mode, got: {other:?}"),
        }
        // The sensitive floor still denies even in approval mode.
        assert!(matches!(
            service
                .workspace_rpc(WorkspaceRequest::ApplyEdit {
                    path: std::path::PathBuf::from(".env"),
                    contents: "SECRET=x".to_string(),
                })
                .await,
            WorkspaceResponse::Error { .. }
        ));
    }

    #[tokio::test]
    async fn workspace_rpc_snapshot_and_list_filter_sensitive_files() {
        use otto_protocol::{WorkspaceRequest, WorkspaceResponse};
        let dir = tempfile::tempdir().unwrap();
        let service = service_in(&dir, crate::build_default_registry()).await;
        // Seed a benign file and a sensitive non-dotfile (id_rsa) directly on disk.
        std::fs::write(dir.path().join("a.txt"), "ok").unwrap();
        std::fs::write(dir.path().join("id_rsa"), "PRIVATE KEY").unwrap();

        match service
            .workspace_rpc(WorkspaceRequest::List {
                glob: "**".to_string(),
            })
            .await
        {
            WorkspaceResponse::List { paths } => {
                assert!(paths.contains(&std::path::PathBuf::from("a.txt")));
                assert!(
                    !paths.iter().any(|p| p.ends_with("id_rsa")),
                    "id_rsa must be gate-filtered from list"
                );
            }
            other => panic!("unexpected: {other:?}"),
        }
        match service.workspace_rpc(WorkspaceRequest::Snapshot).await {
            WorkspaceResponse::Snapshot { files } => {
                assert!(
                    files
                        .iter()
                        .any(|(p, _)| p == &std::path::PathBuf::from("a.txt"))
                );
                assert!(
                    !files.iter().any(|(p, _)| p.ends_with("id_rsa")),
                    "id_rsa must be gate-filtered from snapshot"
                );
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn orchestrator_error_marks_session_failed() {
        let dir = tempfile::tempdir().unwrap();
        // Empty registry: the orchestrator can't find the Planner, so run_turn errors.
        let service = service_in(&dir, AgentRegistry::new()).await;
        let id = service
            .create_session("g", &serde_json::json!({}))
            .await
            .unwrap();
        let mut sink = CollectingSink::default();
        let result = service.run_prompt(id, "g", &mut sink).await;
        assert!(result.is_err());
        assert_eq!(
            service.store().session_status(id).await.unwrap(),
            SessionStatus::Failed
        );
    }
}
