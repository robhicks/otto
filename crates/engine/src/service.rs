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
use otto_engine_core::{AgentRegistry, Orchestrator, Retriever, Router, TokenMeter, TurnOutcome};
use otto_extensions::{Extensions, expand_args, resolve_injections};
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

/// The per-turn control handles the serve layer can inject: how `Ask` edits are approved, how
/// the turn is paused, and (for a `Command::RunCommand` turn) a narrowed tool registry / pinned
/// router that apply to THIS call only — the service's own long-lived `tools`/`router` are never
/// mutated. `Default` is the headless/CLI posture (deny approvals, never pause, no overrides).
pub struct TurnControls {
    pub approver: Arc<dyn Approver>,
    pub pauser: Arc<dyn PauseController>,
    pub tools: Option<Arc<ToolRegistry>>,
    pub router: Option<Arc<dyn Router>>,
}

impl Default for TurnControls {
    fn default() -> Self {
        Self {
            approver: Arc::new(DenyApprover),
            pauser: Arc::new(NeverPause),
            tools: None,
            router: None,
        }
    }
}

/// Outcome of a failed `accept_promotion`, mapped to HTTP status by the `/promote` handler.
#[derive(Debug)]
pub enum AcceptError {
    /// The session id already exists in the receiver store → `409 Conflict` (no silent overwrite).
    AlreadyExists,
    /// The bundle is hostile or malformed (sensitive-path entry, non-UTF-8 path/contents) → `400`.
    /// A client fault: the receiver is fine, so it must not read as a server error or be retried.
    Refused(String),
    /// A genuine receiver-side failure (store/IO error) → `500`.
    Failed(anyhow::Error),
}

/// Runs sessions against a store and a fixed set of engine deps. One turn at a time
/// (`turn_lock`), because the workspace is shared mutable state.
pub struct EngineService {
    store: Arc<dyn SessionStore>,
    registry: Arc<AgentRegistry>,
    router: Arc<dyn Router>,
    workspace: Arc<dyn Workspace>,
    tools: Arc<ToolRegistry>,
    retriever: Option<Arc<dyn Retriever>>,
    extensions: Option<Arc<Extensions>>,
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
            retriever: None,
            extensions: None,
            turn_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// Attach a retriever (the indexed candidate source). `None` keeps the lexical fallback.
    pub fn with_retriever(mut self, retriever: Option<Arc<dyn Retriever>>) -> Self {
        self.retriever = retriever;
        self
    }

    /// Attach the discovered `.claude/` extensions so `run_command_with_controls` (≙
    /// `Command::RunCommand`) can resolve a command by name. `None` (the default — every
    /// existing `EngineService::new` call site) means every `RunCommand` call fails with a
    /// clear error; there is no silent no-op.
    pub fn with_extensions(mut self, extensions: Arc<Extensions>) -> Self {
        self.extensions = Some(extensions);
        self
    }

    /// The session store, for reads the serve layer needs (e.g. replay on reconnect).
    pub fn store(&self) -> &dyn SessionStore {
        &*self.store
    }

    /// The workspace this service edits, for operations that need it directly (e.g. `promote`,
    /// which snapshots the workspace). Agents never get this — they see only the read-only view.
    pub fn workspace(&self) -> &dyn Workspace {
        &*self.workspace
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
            let router = controls
                .router
                .clone()
                .unwrap_or_else(|| Arc::clone(&self.router));
            let workspace = Arc::clone(&self.workspace);
            let tools = controls
                .tools
                .clone()
                .unwrap_or_else(|| Arc::clone(&self.tools));
            let retriever = self.retriever.clone();
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
                    retriever: retriever.as_deref(),
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

    /// Look up `name` in the discovered custom commands (set via `with_extensions`), expand its
    /// template with `args`, resolve `!bash`/`@file` injections through a tool registry narrowed
    /// to its `allowed-tools`, and run the result as a normal turn with a router pinned to its
    /// `model`. (≙ `Command::RunCommand`.) Errors before any turn starts — unknown `name`, or an
    /// injection failure (e.g. the sensitive-path floor denying `@.env`) — so no `seq` is
    /// consumed and the session is untouched.
    pub async fn run_command_with_controls(
        &self,
        session: SessionId,
        name: &str,
        args: &[String],
        sink: &mut dyn EventSink,
        approver: Arc<dyn Approver>,
        pauser: Arc<dyn PauseController>,
    ) -> anyhow::Result<TurnOutcome> {
        let extensions = self.extensions.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "no command named '{name}': this server was not configured with any extensions"
            )
        })?;
        let def = extensions
            .commands
            .iter()
            .find(|c| c.name == name)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "no command named '{name}' in ~/.claude/commands/ or the project .claude/commands/"
                )
            })?;

        let narrowed_tools: Arc<ToolRegistry> = match &def.allowed_tools {
            Some(list) => Arc::new(self.tools.subset(list)),
            None => Arc::clone(&self.tools),
        };

        let expanded = expand_args(&def.template, args);
        let goal = resolve_injections(&expanded, &narrowed_tools).await?;

        let pinned_router: Arc<dyn Router> =
            Arc::from(crate::build_router_with_model(def.model.as_deref()));

        let controls = TurnControls {
            approver,
            pauser,
            tools: Some(narrowed_tools),
            router: Some(pinned_router),
        };
        self.run_prompt_with_controls(session, &goal, sink, controls)
            .await
    }

    /// Validate every workspace file in `bundle` through the permission gate and return the edits
    /// to apply. The inviolable sensitive-path floor is enforced here (fail-closed): a non-UTF-8
    /// path, a path escaping the root, a sensitive path, or non-UTF-8 contents aborts with nothing
    /// applied. Shared by `accept_promotion` (receiver restore) and `accept_demotion` (source pull).
    fn validate_workspace_edits(
        &self,
        bundle: &otto_remote::PromoteBundle,
    ) -> Result<Vec<Edit>, AcceptError> {
        let mut edits = Vec::with_capacity(bundle.workspace.files.len());
        for (path, bytes) in &bundle.workspace.files {
            // The gate is the sensitive-path defense here (`apply_edit` only checks containment),
            // so feed it the LOSSLESS path string — never `to_string_lossy`, whose U+FFFD
            // substitution could let a non-UTF-8 path slip a sensitive marker past the gate.
            // A non-UTF-8 path is itself untrusted input: reject it outright.
            let Some(path_str) = path.to_str() else {
                return Err(AcceptError::Refused(format!(
                    "restore refused non-UTF-8 path: {}",
                    path.display()
                )));
            };
            // Reject path-escape (`..`, absolute, drive prefix) BEFORE restoring the session, so a
            // traversal entry aborts with nothing landing (otherwise `apply_edit`'s containment
            // check would fire only after `store.restore`, leaving an orphaned session row).
            if path.components().any(|c| {
                matches!(
                    c,
                    std::path::Component::ParentDir
                        | std::path::Component::RootDir
                        | std::path::Component::Prefix(_)
                )
            }) {
                return Err(AcceptError::Refused(format!(
                    "restore refused path escaping workspace root: {path_str}"
                )));
            }
            if self.tools.check("fs.write", &json!({ "path": path_str })) == Decision::Deny {
                return Err(AcceptError::Refused(format!(
                    "restore refused sensitive path: {path_str}"
                )));
            }
            let new_contents = String::from_utf8(bytes.clone()).map_err(|_| {
                AcceptError::Refused(format!("restore: non-UTF-8 contents for {path_str}"))
            })?;
            edits.push(Edit {
                path: path.clone(),
                new_contents,
            });
        }
        Ok(edits)
    }

    /// Restore a promoted bundle into this (receiver) engine: write each workspace file through
    /// the permission gate, then restore the session into the store. Fail-closed and validated
    /// up front — a sensitive-path entry (`.env`/`.ssh`/…) is refused before anything is written,
    /// and a duplicate session id is reported (never overwritten).
    pub async fn accept_promotion(
        &self,
        bundle: &otto_remote::PromoteBundle,
    ) -> Result<SessionId, AcceptError> {
        let id = bundle.session.id;

        // Duplicate probe: a present session is a 409, not a silent overwrite. This is only a
        // fast-path label — the real guard is `store.restore`'s atomic INSERT, which fails the
        // primary-key constraint on a duplicate, so a probe that errors for any other reason
        // (and falls through here) still cannot overwrite an existing session.
        if self.store.session_status(id).await.is_ok() {
            return Err(AcceptError::AlreadyExists);
        }

        // Validate the WHOLE workspace snapshot through the gate before writing anything: a
        // sensitive-path entry is refused (fail-closed) and nothing lands.
        let edits = self.validate_workspace_edits(bundle)?;

        // Session first, then the pre-validated workspace files.
        self.store
            .restore(&bundle.session)
            .await
            .map_err(AcceptError::Failed)?;
        for edit in &edits {
            self.workspace
                .apply_edit(edit)
                .await
                .map_err(AcceptError::Failed)?;
        }
        Ok(id)
    }

    /// Restore a bundle pulled back FROM a remote (demote) into this (source) engine, OVERWRITING
    /// this engine's own prior copy of the session. Unlike `accept_promotion`, a present session id
    /// is expected (the source kept its copy when it promoted) and is replaced via `restore_over`.
    /// The sensitive-path floor is still enforced up front (fail-closed) before anything is written.
    pub async fn accept_demotion(
        &self,
        bundle: &otto_remote::PromoteBundle,
    ) -> Result<SessionId, AcceptError> {
        let id = bundle.session.id;
        let edits = self.validate_workspace_edits(bundle)?;
        // Overwrite the source's own (stale) session row, then the pre-validated workspace files.
        self.store
            .restore_over(&bundle.session)
            .await
            .map_err(AcceptError::Failed)?;
        for edit in &edits {
            self.workspace
                .apply_edit(edit)
                .await
                .map_err(AcceptError::Failed)?;
        }
        Ok(id)
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
            WorkspaceRequest::Snapshot => match self.filtered_workspace_snapshot().await {
                Ok(snap) => WorkspaceResponse::Snapshot { files: snap.files },
                Err(e) => WorkspaceResponse::Error {
                    message: e.to_string(),
                },
            },
        }
    }

    /// The workspace snapshot with gate-denied paths removed: any path the read gate denies (the
    /// inviolable sensitive-path floor) is omitted, so secrets never leave this engine. Shared by
    /// the `/workspace` Snapshot RPC and `export_promotion`.
    async fn filtered_workspace_snapshot(
        &self,
    ) -> anyhow::Result<otto_engine_core::types::WorkspaceSnapshot> {
        let snap = self.workspace.snapshot().await?;
        let files = snap
            .files
            .into_iter()
            .filter(|(p, _)| {
                self.tools
                    .check("fs.read", &json!({ "path": p.to_string_lossy() }))
                    == Decision::Allow
            })
            .collect();
        Ok(otto_engine_core::types::WorkspaceSnapshot { files })
    }

    /// Build a `PromoteBundle` for `session` so a demoting source can pull it back. The workspace
    /// snapshot is gate-filtered (sensitive paths excluded — secrets never leave this engine).
    /// Errors if the session is unknown.
    pub async fn export_promotion(
        &self,
        session: SessionId,
    ) -> anyhow::Result<otto_remote::PromoteBundle> {
        Ok(otto_remote::PromoteBundle {
            session: self.store.snapshot(session).await?,
            workspace: self.filtered_workspace_snapshot().await?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use otto_persistence::SqliteStore;
    use otto_providers::ScriptedProvider;
    use otto_router::SingleProviderRouter;
    use otto_workspace::LocalWorkspace;

    /// A receiver-style service over a fresh empty store + temp workspace, offline router.
    async fn build_test_service(
        ws_root: &std::path::Path,
        db_path: std::path::PathBuf,
    ) -> EngineService {
        let workspace: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(ws_root));
        let tools_ws: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(ws_root));
        let tools = Arc::new(crate::build_tool_registry(tools_ws, ws_root.to_path_buf()));
        let store: Arc<dyn SessionStore> = Arc::new(SqliteStore::open(db_path).await.unwrap());
        let router: Arc<dyn Router> = Arc::from(crate::build_router());
        EngineService::new(
            store,
            Arc::new(crate::build_default_registry()),
            router,
            workspace,
            tools,
        )
    }

    #[tokio::test]
    async fn export_promotion_returns_bundle_without_sensitive_files() {
        let ws = tempfile::tempdir().unwrap();
        let db = tempfile::tempdir().unwrap();
        // Put a normal file and a sensitive file on disk in the workspace.
        std::fs::write(ws.path().join("out.txt"), b"KEEP").unwrap();
        std::fs::write(ws.path().join(".env"), b"SECRET=1").unwrap();
        let service = build_test_service(ws.path(), db.path().join("s.db")).await;
        let id = service
            .create_session("g", &serde_json::json!({}))
            .await
            .unwrap();

        let bundle = service.export_promotion(id).await.unwrap();
        assert_eq!(bundle.session.id, id);
        let paths: Vec<_> = bundle
            .workspace
            .files
            .iter()
            .map(|(p, _)| p.clone())
            .collect();
        assert!(paths.contains(&std::path::PathBuf::from("out.txt")));
        // The sensitive-path floor filtered .env out of the export — it never leaves the receiver.
        assert!(!paths.contains(&std::path::PathBuf::from(".env")));
    }

    #[tokio::test]
    async fn export_promotion_unknown_session_is_error() {
        let ws = tempfile::tempdir().unwrap();
        let db = tempfile::tempdir().unwrap();
        let service = build_test_service(ws.path(), db.path().join("s.db")).await;
        assert!(service.export_promotion(SessionId::new()).await.is_err());
    }

    #[tokio::test]
    async fn accept_promotion_restores_session_and_workspace() {
        use otto_engine_core::types::WorkspaceSnapshot;
        use otto_persistence::SessionState;
        use otto_remote::PromoteBundle;
        use std::path::PathBuf;

        let ws_dir = tempfile::tempdir().unwrap();
        let db_dir = tempfile::tempdir().unwrap();
        let service = build_test_service(ws_dir.path(), db_dir.path().join("r.db")).await;

        let id = SessionId::new();
        let bundle = PromoteBundle {
            session: SessionState {
                id,
                goal: "g".to_string(),
                status: SessionStatus::Active,
                config: serde_json::json!({}),
                events: vec![],
                turns: vec![],
            },
            workspace: WorkspaceSnapshot {
                files: vec![(PathBuf::from("out.txt"), b"HELLO".to_vec())],
            },
        };

        let restored = service.accept_promotion(&bundle).await.unwrap();
        assert_eq!(restored, id);
        assert!(service.store().session_status(id).await.is_ok());
        assert_eq!(
            service
                .workspace()
                .read(std::path::Path::new("out.txt"))
                .await
                .unwrap(),
            b"HELLO"
        );
    }

    #[tokio::test]
    async fn accept_promotion_refuses_sensitive_workspace_entry() {
        use otto_engine_core::types::WorkspaceSnapshot;
        use otto_persistence::SessionState;
        use otto_remote::PromoteBundle;
        use std::path::PathBuf;

        let ws_dir = tempfile::tempdir().unwrap();
        let db_dir = tempfile::tempdir().unwrap();
        let service = build_test_service(ws_dir.path(), db_dir.path().join("r.db")).await;

        let id = SessionId::new();
        let bundle = PromoteBundle {
            session: SessionState {
                id,
                goal: "g".to_string(),
                status: SessionStatus::Active,
                config: serde_json::json!({}),
                events: vec![],
                turns: vec![],
            },
            workspace: WorkspaceSnapshot {
                files: vec![(PathBuf::from(".env"), b"SECRET=1".to_vec())],
            },
        };

        let err = service.accept_promotion(&bundle).await;
        // A sensitive entry is a client fault (Refused → 400), not a receiver error.
        assert!(matches!(err, Err(AcceptError::Refused(_))));
        // Fail-closed: nothing landed — neither the file nor the session.
        assert!(
            service
                .workspace()
                .read(std::path::Path::new(".env"))
                .await
                .is_err()
        );
        assert!(service.store().session_status(id).await.is_err());
    }

    /// Build a one-file bundle for the edge-case restore tests below.
    fn bundle_with(
        id: SessionId,
        path: std::path::PathBuf,
        bytes: Vec<u8>,
    ) -> otto_remote::PromoteBundle {
        use otto_engine_core::types::WorkspaceSnapshot;
        use otto_persistence::SessionState;
        otto_remote::PromoteBundle {
            session: SessionState {
                id,
                goal: "g".to_string(),
                status: SessionStatus::Active,
                config: serde_json::json!({}),
                events: vec![],
                turns: vec![],
            },
            workspace: WorkspaceSnapshot {
                files: vec![(path, bytes)],
            },
        }
    }

    #[tokio::test]
    async fn accept_promotion_refuses_parent_dir_escape() {
        use std::path::PathBuf;
        let ws_dir = tempfile::tempdir().unwrap();
        let db_dir = tempfile::tempdir().unwrap();
        let service = build_test_service(ws_dir.path(), db_dir.path().join("r.db")).await;

        let id = SessionId::new();
        let bundle = bundle_with(id, PathBuf::from("../escape.txt"), b"x".to_vec());
        // A traversal path is refused up front (400) — nothing lands outside the root, and the
        // session is NOT restored (rejected before `store.restore`).
        assert!(matches!(
            service.accept_promotion(&bundle).await,
            Err(AcceptError::Refused(_))
        ));
        assert!(!ws_dir.path().parent().unwrap().join("escape.txt").exists());
        assert!(service.store().session_status(id).await.is_err());
    }

    #[tokio::test]
    async fn accept_promotion_refuses_absolute_path() {
        use std::path::PathBuf;
        let ws_dir = tempfile::tempdir().unwrap();
        let db_dir = tempfile::tempdir().unwrap();
        let service = build_test_service(ws_dir.path(), db_dir.path().join("r.db")).await;

        let id = SessionId::new();
        let bundle = bundle_with(id, PathBuf::from("/tmp/otto-escape.txt"), b"x".to_vec());
        assert!(matches!(
            service.accept_promotion(&bundle).await,
            Err(AcceptError::Refused(_))
        ));
        assert!(service.store().session_status(id).await.is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn accept_promotion_refuses_non_utf8_path() {
        use std::os::unix::ffi::OsStrExt;
        use std::path::PathBuf;
        let ws_dir = tempfile::tempdir().unwrap();
        let db_dir = tempfile::tempdir().unwrap();
        let service = build_test_service(ws_dir.path(), db_dir.path().join("r.db")).await;

        let id = SessionId::new();
        // A path with non-UTF-8 bytes is untrusted input: it must be refused outright, never
        // gate-checked via a lossy string (which could mask a sensitive marker).
        let path = PathBuf::from(std::ffi::OsStr::from_bytes(b"weird\xFFname"));
        let bundle = bundle_with(id, path, b"x".to_vec());
        assert!(matches!(
            service.accept_promotion(&bundle).await,
            Err(AcceptError::Refused(_))
        ));
        assert!(service.store().session_status(id).await.is_err());
    }

    #[tokio::test]
    async fn accept_promotion_refuses_non_utf8_contents() {
        use std::path::PathBuf;
        let ws_dir = tempfile::tempdir().unwrap();
        let db_dir = tempfile::tempdir().unwrap();
        let service = build_test_service(ws_dir.path(), db_dir.path().join("r.db")).await;

        let id = SessionId::new();
        let bundle = bundle_with(id, PathBuf::from("bin.dat"), vec![0xFF, 0xFE]);
        let err = service.accept_promotion(&bundle).await;
        assert!(matches!(err, Err(AcceptError::Refused(_))));
        // Fail-closed: the validation loop runs before any write, so nothing landed.
        assert!(
            service
                .workspace()
                .read(std::path::Path::new("bin.dat"))
                .await
                .is_err()
        );
        assert!(service.store().session_status(id).await.is_err());
    }

    #[tokio::test]
    async fn accept_promotion_duplicate_session_is_already_exists() {
        use otto_engine_core::types::WorkspaceSnapshot;
        use otto_remote::PromoteBundle;

        let ws_dir = tempfile::tempdir().unwrap();
        let db_dir = tempfile::tempdir().unwrap();
        let service = build_test_service(ws_dir.path(), db_dir.path().join("r.db")).await;

        let id = service
            .create_session("g", &serde_json::json!({}))
            .await
            .unwrap();
        let state = service.store().snapshot(id).await.unwrap();
        let bundle = PromoteBundle {
            session: state,
            workspace: WorkspaceSnapshot { files: vec![] },
        };

        assert!(matches!(
            service.accept_promotion(&bundle).await,
            Err(AcceptError::AlreadyExists)
        ));
    }

    #[tokio::test]
    async fn accept_demotion_overwrites_an_existing_session() {
        use otto_engine_core::types::WorkspaceSnapshot;
        use otto_persistence::SessionState;
        use otto_remote::PromoteBundle;

        let ws = tempfile::tempdir().unwrap();
        let db = tempfile::tempdir().unwrap();
        let service = build_test_service(ws.path(), db.path().join("s.db")).await;

        // Seed S with an original copy of the session.
        let id = service
            .create_session("old", &serde_json::json!({}))
            .await
            .unwrap();

        // A bundle for the SAME id carrying advanced state + a new workspace file.
        let bundle = PromoteBundle {
            session: SessionState {
                id,
                goal: "advanced".to_string(),
                status: otto_persistence::SessionStatus::Done,
                config: serde_json::json!({}),
                events: vec![],
                turns: vec![],
            },
            workspace: WorkspaceSnapshot {
                files: vec![(std::path::PathBuf::from("out.txt"), b"PULLED".to_vec())],
            },
        };

        // accept_demotion overwrites S's stale row (no AlreadyExists), and writes the file.
        let restored = service.accept_demotion(&bundle).await.unwrap();
        assert_eq!(restored, id);
        assert_eq!(service.store().snapshot(id).await.unwrap().goal, "advanced");
        assert_eq!(
            service
                .workspace()
                .read(std::path::Path::new("out.txt"))
                .await
                .unwrap(),
            b"PULLED"
        );
    }

    #[tokio::test]
    async fn accept_demotion_refuses_sensitive_workspace_entry() {
        let ws = tempfile::tempdir().unwrap();
        let db = tempfile::tempdir().unwrap();
        let service = build_test_service(ws.path(), db.path().join("s.db")).await;

        let id = SessionId::new();
        let bundle = bundle_with(id, std::path::PathBuf::from(".env"), b"SECRET=1".to_vec());
        assert!(matches!(
            service.accept_demotion(&bundle).await,
            Err(crate::service::AcceptError::Refused(_))
        ));
        // Fail-closed: nothing landed — neither the file nor the session.
        assert!(
            service
                .workspace()
                .read(std::path::Path::new(".env"))
                .await
                .is_err()
        );
        assert!(service.store().session_status(id).await.is_err());
    }

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

    fn command_def(
        name: &str,
        template: &str,
        allowed_tools: Option<Vec<String>>,
    ) -> otto_extensions::CustomCommandDef {
        otto_extensions::CustomCommandDef {
            name: name.to_string(),
            description: None,
            argument_hint: None,
            model: None,
            allowed_tools,
            template: template.to_string(),
        }
    }

    #[tokio::test]
    async fn run_command_with_controls_unknown_name_errors_without_starting_a_turn() {
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
            .run_command_with_controls(
                id,
                "nope",
                &[],
                &mut sink,
                Arc::new(DenyApprover),
                Arc::new(NeverPause),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no command named 'nope'"));

        let replayed = service.store().replay_since(id, None).await.unwrap();
        assert!(replayed.is_empty(), "no turn should have started");
    }

    #[tokio::test]
    async fn run_command_with_controls_expands_args_and_narrows_tools() {
        let dir = tempfile::tempdir().unwrap();
        let def = command_def("greet", "do $1", Some(vec!["fs.read".to_string()]));
        let extensions = Extensions {
            commands: vec![def],
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
            .run_command_with_controls(
                id,
                "greet",
                &["thing".to_string()],
                &mut sink,
                Arc::new(DenyApprover),
                Arc::new(NeverPause),
            )
            .await
            .unwrap();
        assert!(outcome.ok);

        // Expansion: the recorded turn's goal is the expanded template, not the raw one.
        let state = service.store().snapshot(id).await.unwrap();
        assert_eq!(state.turns.last().unwrap().goal, "do thing");

        // Narrowing: fs.write was excluded, so the Coder's edit was never applied.
        assert!(
            service
                .workspace()
                .read(std::path::Path::new("out.txt"))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn run_command_with_controls_injection_failure_errors_without_starting_a_turn() {
        let dir = tempfile::tempdir().unwrap();
        let def = command_def("leak", "secret: @.env", None);
        let extensions = Extensions {
            commands: vec![def],
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
        let result = service
            .run_command_with_controls(
                id,
                "leak",
                &[],
                &mut sink,
                Arc::new(DenyApprover),
                Arc::new(NeverPause),
            )
            .await;
        assert!(
            result.is_err(),
            "the sensitive-path floor must fail the @.env injection closed"
        );

        let replayed = service.store().replay_since(id, None).await.unwrap();
        assert!(replayed.is_empty(), "no turn should have started");
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
    async fn run_prompt_with_controls_tools_override_restricts_available_tools() {
        let dir = tempfile::tempdir().unwrap();
        let service = service_in(&dir, crate::build_default_registry()).await;
        let id = service
            .create_session("g", &serde_json::json!({}))
            .await
            .unwrap();

        // Narrow to a tool set that excludes fs.write — the Coder's edit-apply check must Deny
        // (ToolRegistry::check denies a tool absent from a subset-narrowed registry).
        let ws: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(dir.path()));
        let base = crate::build_tool_registry(ws, dir.path().to_path_buf());
        let narrowed = Arc::new(base.subset(&["fs.read".to_string()]));

        let controls = TurnControls {
            approver: Arc::new(DenyApprover),
            pauser: Arc::new(NeverPause),
            tools: Some(narrowed),
            router: None,
        };
        let mut sink = CollectingSink::default();
        service
            .run_prompt_with_controls(id, "g", &mut sink, controls)
            .await
            .unwrap();

        // Without fs.write, the Coder's edit was never applied — proving the override, not
        // the service's own (unnarrowed) `self.tools`, is what the orchestrator actually used.
        assert!(
            service
                .workspace()
                .read(std::path::Path::new("out.txt"))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn run_prompt_with_controls_router_override_takes_precedence_over_service_default() {
        let dir = tempfile::tempdir().unwrap();
        // The service's OWN default router (scripted_router()) would write "hi g".
        let service = service_in(&dir, crate::build_default_registry()).await;
        let id = service
            .create_session("g", &serde_json::json!({}))
            .await
            .unwrap();

        // A distinctly different override router.
        let override_provider = ScriptedProvider::new("{}")
            .on(
                "edits",
                r#"{"edits": [{"path": "out.txt", "contents": "OVERRIDDEN"}]}"#,
            )
            .on("milestones", r#"{"milestones": [{"description": "x"}]}"#);
        let override_router: Arc<dyn Router> =
            Arc::new(SingleProviderRouter::new(Arc::new(override_provider)));

        let controls = TurnControls {
            approver: Arc::new(DenyApprover),
            pauser: Arc::new(NeverPause),
            tools: None,
            router: Some(override_router),
        };
        let mut sink = CollectingSink::default();
        service
            .run_prompt_with_controls(id, "g", &mut sink, controls)
            .await
            .unwrap();

        let contents = service
            .workspace()
            .read(std::path::Path::new("out.txt"))
            .await
            .unwrap();
        assert_eq!(contents, b"OVERRIDDEN");
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
    async fn workspace_accessor_reads_written_file() {
        use otto_protocol::{WorkspaceRequest, WorkspaceResponse};
        let dir = tempfile::tempdir().unwrap();
        let service = service_in(&dir, crate::build_default_registry()).await;
        // Write through the RPC, then read back through the accessor.
        assert!(matches!(
            service
                .workspace_rpc(WorkspaceRequest::ApplyEdit {
                    path: std::path::PathBuf::from("a.txt"),
                    contents: "hi".to_string(),
                })
                .await,
            WorkspaceResponse::ApplyEdit { .. }
        ));
        let bytes = service
            .workspace()
            .read(std::path::Path::new("a.txt"))
            .await
            .unwrap();
        assert_eq!(bytes, b"hi".to_vec());
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
