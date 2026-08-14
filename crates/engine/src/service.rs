//! `EngineService`: the transport-agnostic core that runs prompts with live event streaming
//! and persistence. Owns the session store and the shared engine deps; both the CLI and the
//! serve layer drive it. Maps the protocol commands onto operations: `CreateSession` ->
//! `create_session`, `SendPrompt` -> `run_prompt`, `Abort` -> `abort`.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use otto_engine_core::tool::{
    Approver, Decision, DenyApprover, NeverPause, PauseController, Tool, ToolRegistry,
};
use otto_engine_core::traits::{Workspace, WorkspaceRead};
use otto_engine_core::types::{Edit, SessionHistory, TurnSummary};
use otto_engine_core::{AgentRegistry, Orchestrator, Retriever, Router, TokenMeter, TurnOutcome};
use otto_extensions::{Extensions, MarkdownAgent, TaskTool, expand_args, resolve_injections};
use otto_persistence::{SessionStatus, SessionStore, TurnRecord};
use otto_protocol::{Event, EventKind, Role, SessionId, WorkspaceRequest, WorkspaceResponse};
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

    /// The authorization choke point for session access. Returns the same error a nonexistent
    /// session produces, so a caller cannot tell "not yours" from "not there".
    ///
    /// Every client-facing method **on this type** calls it before touching a session. It is not
    /// automatic for the whole server: the handover path in `serve.rs` reaches
    /// `otto_remote::promote` through the `store()` accessor rather than through an
    /// `EngineService` method, so it must call [`Self::authorize_session`] explicitly — which it
    /// does. Any future path that takes a `SessionId` from a client and bypasses this type owes
    /// the same call.
    ///
    /// `store.owner_of` is an unscoped reverse existence oracle — it DOES distinguish the two —
    /// so its error must never reach a client. That is why both arms below bail with the same
    /// message, and why this is the only client-facing caller of `owner_of`.
    async fn authorize(
        &self,
        owner: &otto_protocol::UserId,
        session: SessionId,
    ) -> anyhow::Result<()> {
        match self.store.owner_of(session).await {
            Ok(actual) if &actual == owner => Ok(()),
            // Both arms produce the same message on purpose.
            _ => anyhow::bail!("no session {}", session.0),
        }
    }

    /// [`Self::authorize`] for callers outside this type — specifically `serve.rs`'s handover
    /// path, which does not route through an `EngineService` method and would otherwise be the
    /// one client-facing command that skips the check.
    pub async fn authorize_session(
        &self,
        owner: &otto_protocol::UserId,
        session: SessionId,
    ) -> anyhow::Result<()> {
        self.authorize(owner, session).await
    }

    /// Create and persist a new session owned by `owner`. (≙ `Command::CreateSession`.)
    pub async fn create_session(
        &self,
        owner: &otto_protocol::UserId,
        goal: &str,
        config: &serde_json::Value,
    ) -> anyhow::Result<SessionId> {
        self.store.create_session(owner, goal, config).await
    }

    /// Mark a session aborted. (≙ `Command::Abort`.)
    pub async fn abort(
        &self,
        owner: &otto_protocol::UserId,
        session: SessionId,
    ) -> anyhow::Result<()> {
        self.authorize(owner, session).await?;
        self.store.set_status(session, SessionStatus::Aborted).await
    }

    /// Run one turn with the headless defaults (deny approvals, never pause). (≙ `SendPrompt`.)
    pub async fn run_prompt(
        &self,
        owner: &otto_protocol::UserId,
        session: SessionId,
        goal: &str,
        sink: &mut dyn EventSink,
    ) -> anyhow::Result<TurnOutcome> {
        self.authorize(owner, session).await?;
        self.run_prompt_with_controls(owner, session, goal, sink, TurnControls::default())
            .await
    }

    /// Run one orchestrator turn for `goal`, streaming each event to `sink` after persisting it
    /// (fail-closed), recording the turn, and updating status. `controls` supply the approver
    /// and pause controller. The seq sequence continues from the store. One turn at a time.
    ///
    /// `run_prompt` delegates here, and `run_command_with_controls` runs through it too, so
    /// `authorize` (and its `owner_of` query) fires two or three times per turn. That is harmless
    /// and deliberate — each entry point must be safe on its own. Do not remove the inner call.
    pub async fn run_prompt_with_controls(
        &self,
        owner: &otto_protocol::UserId,
        session: SessionId,
        goal: &str,
        sink: &mut dyn EventSink,
        controls: TurnControls,
    ) -> anyhow::Result<TurnOutcome> {
        // Before the turn lock, before the first store write, before any event: a rejected call
        // must leave no trace.
        self.authorize(owner, session).await?;

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
                    // Serialize the whole outcome, not a hand-built object: conversation history
                    // is rebuilt from these rows, so a field added to `TurnOutcome` must reach
                    // the store without anyone having to remember to widen a `json!()` literal
                    // here.
                    outcome: serde_json::to_value(&outcome)?,
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
    /// injection failure (e.g. the sensitive-path floor denying `@.env`), or no extensions
    /// having been attached via `with_extensions` at all — so no `seq` is consumed and the
    /// session is untouched.
    // The principal pushes this one past clippy's 7-argument threshold. Folding `approver`/
    // `pauser` into a `TurnControls` would fix the count, but this method builds the rest of the
    // `TurnControls` itself (the narrowed tools + pinned router), so a caller-supplied one would
    // be half-ignored — a worse signature than a long one.
    #[allow(clippy::too_many_arguments)]
    pub async fn run_command_with_controls(
        &self,
        owner: &otto_protocol::UserId,
        session: SessionId,
        name: &str,
        args: &[String],
        sink: &mut dyn EventSink,
        approver: Arc<dyn Approver>,
        pauser: Arc<dyn PauseController>,
    ) -> anyhow::Result<TurnOutcome> {
        self.authorize(owner, session).await?;

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

        // Always rebuilt from the environment (never falls back to self.router), matching
        // otto run --command's model-pinning behavior exactly — a command with model: None
        // still gets a fresh router built the same way SendPrompt's default router would be,
        // not necessarily the SAME instance as self.router.
        let pinned_router: Arc<dyn Router> =
            Arc::from(crate::build_router_with_model(def.model.as_deref()));

        let controls = TurnControls {
            approver,
            pauser,
            tools: Some(narrowed_tools),
            router: Some(pinned_router),
        };
        self.run_prompt_with_controls(owner, session, &goal, sink, controls)
            .await
    }

    /// Look up `name` in the discovered custom agents (set via `with_extensions`), dispatch it as
    /// a single, non-interruptible `TaskTool`/`MarkdownAgent` call (no `Orchestrator::run_turn`),
    /// and synthesize the turn's event stream from the existing `EventKind` vocabulary:
    /// `AgentStarted`, `Log` (the agent's response text, or the dispatch error on failure),
    /// `AgentFinished`, `TurnComplete`. (≙ `Command::RunAgent`.) Errors before any event is
    /// emitted — unknown `name`, or no extensions attached via `with_extensions` at all — so no
    /// `seq` is consumed and the session is untouched. A dispatch failure (e.g. a provider error)
    /// is reported as `Ok(TurnOutcome { ok: false })`, not `Err`: it still emits
    /// `AgentFinished`/`TurnComplete { ok: false }` and marks the session `Failed`, exactly like
    /// an orchestrator verify-failure does for `SendPrompt`/`RunCommand` — `Err` is reserved for
    /// failures that happen *before* `TurnComplete` is ever emitted (e.g. a persist failure inside
    /// `emit_and_persist`), matching `Orchestrator::run_turn`'s own contract. This keeps
    /// `serve.rs`'s `report_turn_outcome` from ever sending a `ServerMessage::Error` frame after a
    /// `TurnComplete` has already streamed for the same call.
    pub async fn run_agent_with_controls(
        &self,
        owner: &otto_protocol::UserId,
        session: SessionId,
        name: &str,
        prompt: &str,
        sink: &mut dyn EventSink,
    ) -> anyhow::Result<TurnOutcome> {
        self.authorize(owner, session).await?;
        self.run_agent_with_controls_inner(session, name, prompt, sink, None)
            .await
    }

    /// `run_agent_with_controls`'s real implementation, parameterized by an optional router
    /// override used only by tests (see the test module) to substitute a failing `Router` and
    /// exercise the `task.call` error path deterministically and offline —
    /// `build_router_with_model` reads `ANTHROPIC_API_KEY` and, with no override, would
    /// otherwise require real network I/O to ever fail. Every real caller is
    /// `run_agent_with_controls` above, which always passes `None`, so production behavior is
    /// byte-for-byte unchanged: the router is always freshly built via `build_router_with_model`,
    /// never falling back to `self.router`.
    async fn run_agent_with_controls_inner(
        &self,
        session: SessionId,
        name: &str,
        prompt: &str,
        sink: &mut dyn EventSink,
        router_override: Option<Arc<dyn Router>>,
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
        // run_command_with_controls's model-pinning convention exactly — unless a test supplied
        // an override (see the doc comment above).
        let router: Arc<dyn Router> = match router_override {
            Some(r) => r,
            None => Arc::from(crate::build_router_with_model(model_override.as_deref())),
        };
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

        // From here, seq/turn_index are reserved: any failure past this point still marks the
        // session Failed, matching run_prompt_with_controls's contract for an in-flight turn.
        let outcome = self
            .run_agent_dispatch(session, &counter, name, prompt, &task, sink)
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
    /// failure is reported *in-band*, as a `Log` carrying the error text followed by
    /// `TurnComplete { ok: false }` — this function always returns `Ok(TurnOutcome { ok })`, never
    /// propagating the dispatch error as `Err`. Only a failure in `emit_and_persist` itself (a
    /// persist/stream error, via `?`) returns `Err` here, which — same as
    /// `Orchestrator::run_turn` — can only happen before the fixed sequence completes, never
    /// after a `TurnComplete` has already streamed.
    async fn run_agent_dispatch(
        &self,
        session: SessionId,
        counter: &AtomicU64,
        name: &str,
        prompt: &str,
        task: &TaskTool,
        sink: &mut dyn EventSink,
    ) -> anyhow::Result<TurnOutcome> {
        let role = Role::Custom(name.to_string());
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

        let ok = dispatch.is_ok();
        let log_message = match dispatch {
            Ok(text) => text,
            Err(e) => format!("agent dispatch failed: {e}"),
        };
        self.emit_and_persist(
            session,
            counter,
            sink,
            EventKind::Log {
                message: log_message,
            },
        )
        .await?;

        self.emit_and_persist(
            session,
            counter,
            sink,
            EventKind::AgentFinished { role: role.clone() },
        )
        .await?;

        self.emit_and_persist(session, counter, sink, EventKind::TurnComplete { ok })
            .await?;

        Ok(TurnOutcome {
            ok,
            milestones: Vec::new(),
            files_edited: Vec::new(),
            verify: None,
        })
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
        //
        // Scoping this probe by the bundle's owner narrows it: a session id that already exists
        // under a DIFFERENT owner no longer reports AlreadyExists, and instead falls through to
        // `restore`'s primary-key failure and a generic error. Unreachable while `local` is the
        // only principal; slice 1b must decide whether that is the behavior it wants.
        if self
            .store
            .session_status(&bundle.session.owner, id)
            .await
            .is_ok()
        {
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
        expected: SessionId,
        bundle: &otto_remote::PromoteBundle,
    ) -> Result<SessionId, AcceptError> {
        let id = bundle.session.id;
        // The bundle is whatever the remote chose to return, and `restore_over` is an
        // unconditional overwrite — so without this check a receiver could answer an export for
        // session X with a bundle for session Y and overwrite Y's row, including its `owner`,
        // on the source. Bind the response to the request.
        if id != expected {
            return Err(AcceptError::Refused(format!(
                "demotion bundle is for session {}, but {} was requested",
                id.0, expected.0
            )));
        }
        // `restore_over` overwrites the row INCLUDING its owner, so the id binding alone is not
        // enough: a tampered bundle carrying the right id but a different owner would reassign
        // the local row's ownership. Refuse it. `owner_of` is a reverse-existence oracle, so a
        // demotion for an unknown local row fails closed here too — its error is a genuine
        // store-side failure (`Failed`), surfaced (as store text) only to the already-authorized
        // WS caller, with the Refused text re-deriving both ids from the known-good `expected`.
        let current_owner = self
            .store
            .owner_of(expected)
            .await
            .map_err(AcceptError::Failed)?;
        if bundle.session.owner != current_owner {
            return Err(AcceptError::Refused(format!(
                "demotion bundle is owned by {}, but the local copy of {} is owned by {}",
                bundle.session.owner.as_str(),
                expected.0,
                current_owner.as_str(),
            )));
        }
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
        // Machine-to-machine: there is no connected principal. The owner is derived from the
        // session purely to satisfy the owner-scoped `snapshot`; passing it back in as an
        // authorization check would be a tautology.
        //
        // The ownership check for handover lives on the source, in `serve.rs`'s WS command loop
        // where a principal exists: the `PromoteToRemote`/`DemoteToLocal` arm calls
        // `EngineService::authorize_session` before `handle_handover`. It is there rather than
        // here because this function's only non-test caller is `POST /export`, which is
        // machine-to-machine and has no connected principal.
        //
        // Still unchecked, and deliberately so until the identity slice:
        //   - `POST /export` is bearer-only, so any token holder can export any session id.
        //   - `POST /promote` is bearer-only and takes the owner from the pushed bundle, so a
        //     token holder can plant a session row owned by an arbitrary principal. Unlike the
        //     other two this is not merely inert — it is a live authorization concern the moment
        //     real principals exist, since nothing today enumerates sessions by owner.
        //   - `resolve_session`'s explicit `?session=` arm attaches without an ownership check.
        //     Inert: every command then routes through `authorize`, and connect-time replay is
        //     owner-scoped, so an attach to a foreign session yields `Ready` and nothing else.
        let owner = self.store.owner_of(session).await?;
        Ok(otto_remote::PromoteBundle {
            session: self.store.snapshot(&owner, session).await?,
            workspace: self.filtered_workspace_snapshot().await?,
        })
    }
}

/// Fold persisted turn records into the bounded history the spine hands to agents.
///
/// A record whose `outcome` will not deserialize as a `TurnOutcome` is **skipped**, not fatal:
/// history is an optimization, and one unreadable row from an older or corrupted store must
/// never stop a turn from running.
///
/// Not yet called from production code: `run_prompt_with_controls` starts passing real history
/// to `Orchestrator::run_turn` once that call gains a `history` parameter (a later task in this
/// plan). Until then this is exercised only by the test below.
#[allow(dead_code)]
pub(crate) fn history_from_records(records: Vec<TurnRecord>) -> SessionHistory {
    let summaries = records
        .into_iter()
        .filter_map(|r| {
            let outcome: TurnOutcome = serde_json::from_value(r.outcome).ok()?;
            Some(TurnSummary {
                turn_index: r.turn_index,
                goal: r.goal,
                milestones: outcome.milestones,
                files_edited: outcome.files_edited,
                verify: outcome.verify,
                ok: outcome.ok,
            })
        })
        .collect();
    SessionHistory::new(summaries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use otto_persistence::SqliteStore;
    use otto_providers::ScriptedProvider;
    use otto_router::SingleProviderRouter;
    use otto_workspace::LocalWorkspace;

    /// The reserved principal every test that is not specifically about ownership uses.
    fn local() -> otto_protocol::UserId {
        otto_protocol::UserId::local()
    }

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

    /// `build_test_service` over a fresh temp workspace + database, returning the `TempDir` too:
    /// it must stay bound for the test's lifetime or the database file is deleted mid-test.
    async fn build_service_for_test() -> (EngineService, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let ws_root = dir.path().join("ws");
        std::fs::create_dir_all(&ws_root).unwrap();
        let service = build_test_service(&ws_root, dir.path().join("s.db")).await;
        (service, dir)
    }

    #[tokio::test]
    async fn client_facing_methods_reject_a_non_owner() {
        let (service, _tmp) = build_service_for_test().await;
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

        // Not just "it errored": the rejected call must not have *done* anything. Asserting
        // only `is_err()` here let a mutant that ran `set_status` and then authorized survive
        // the whole suite — a non-owner aborting someone else's session while still getting an
        // Err back. Read the state through.
        assert!(service.abort(&bob, session).await.is_err());
        assert_eq!(
            service.store.session_status(&alice, session).await.unwrap(),
            otto_persistence::SessionStatus::Active,
            "a rejected abort must leave the session untouched"
        );
        assert_eq!(
            service.store.next_seq(session).await.unwrap(),
            0,
            "a rejected turn must not consume a seq"
        );

        // ...and the owner still works.
        assert!(service.abort(&alice, session).await.is_ok());
    }

    /// `serve.rs` dispatches `Command::SendPrompt` straight to `run_prompt_with_controls` — it
    /// does NOT go through `run_prompt` — so this method's own `authorize` call is the served
    /// hot path's only authorization. It needs its own test: deleting that inner call left the
    /// entire suite green, because every other test reached it through `run_prompt`'s check.
    #[tokio::test]
    async fn run_prompt_with_controls_rejects_a_non_owner_directly() {
        let (service, _tmp) = build_service_for_test().await;
        let alice = otto_protocol::UserId::parse("alice").unwrap();
        let bob = otto_protocol::UserId::parse("bob").unwrap();
        let session = service
            .create_session(&alice, "g", &serde_json::json!({}))
            .await
            .unwrap();

        let mut sink = CollectingSink::default();
        let err = service
            .run_prompt_with_controls(
                &bob,
                session,
                "go",
                &mut sink,
                TurnControls {
                    approver: Arc::new(DenyApprover),
                    pauser: Arc::new(NeverPause),
                    tools: None,
                    router: None,
                },
            )
            .await
            .unwrap_err()
            .to_string();

        assert!(err.contains("no session"), "unexpected: {err}");
        assert!(sink.events.is_empty(), "a rejected turn must emit nothing");
        assert_eq!(
            service.store.next_seq(session).await.unwrap(),
            0,
            "a rejected turn must not consume a seq"
        );
        assert_eq!(
            service.store.next_turn(session).await.unwrap(),
            0,
            "a rejected turn must not consume a turn index"
        );
        assert_eq!(
            service.store.session_status(&alice, session).await.unwrap(),
            otto_persistence::SessionStatus::Active,
            "a rejected turn must not change the session's status"
        );
    }

    /// `record_turn` must persist the whole `TurnOutcome`, not a hand-built `{"ok": ...}` —
    /// conversation history is rebuilt from these rows (a later task), so every field on
    /// `TurnOutcome` has to survive the round trip through the store without anyone having to
    /// remember to widen a `json!()` literal at the call site.
    #[tokio::test]
    async fn record_turn_persists_the_full_outcome_not_just_ok() {
        let ws = tempfile::tempdir().unwrap();
        let db = tempfile::tempdir().unwrap();
        let store: Arc<dyn SessionStore> =
            Arc::new(SqliteStore::open(db.path().join("s.db")).await.unwrap());
        let workspace: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(ws.path()));
        let tools_ws: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(ws.path()));
        let tools = Arc::new(crate::build_tool_registry(
            tools_ws,
            ws.path().to_path_buf(),
        ));
        let service = EngineService::new(
            Arc::clone(&store),
            Arc::new(crate::build_default_registry()),
            Arc::from(crate::build_router()),
            workspace,
            tools,
        );

        let owner = local();
        let session = service
            .create_session(&owner, "goal", &serde_json::json!({}))
            .await
            .unwrap();
        let mut sink = CollectingSink::default();
        service
            .run_prompt(&owner, session, "add a hello function", &mut sink)
            .await
            .unwrap();

        let state = store.snapshot(&owner, session).await.unwrap();
        let stored = &state.turns[0].outcome;
        let parsed: TurnOutcome = serde_json::from_value(stored.clone()).unwrap();

        assert!(
            !parsed.milestones.is_empty(),
            "milestones must reach the store; record_turn built a hand-rolled json!() before this change"
        );
        assert!(stored.get("files_edited").is_some());
        assert!(stored.get("verify").is_some());
    }

    /// `run_command_with_controls` and `run_agent_with_controls` authorize before looking the
    /// artifact up, and this asserts the *ordering*, not merely that a check exists: the service
    /// under test has no extensions configured, so the owner fails with the extensions error
    /// while a non-owner fails with `no session`. If `authorize` were ever moved below the
    /// lookup, the non-owner would start getting the extensions error and this test would fail.
    #[tokio::test]
    async fn command_and_agent_dispatch_authorize_before_looking_anything_up() {
        let (service, _tmp) = build_service_for_test().await;
        let alice = otto_protocol::UserId::parse("alice").unwrap();
        let bob = otto_protocol::UserId::parse("bob").unwrap();
        let session = service
            .create_session(&alice, "g", &serde_json::json!({}))
            .await
            .unwrap();

        let mut sink = CollectingSink::default();
        let err = service
            .run_command_with_controls(
                &bob,
                session,
                "anything",
                &[],
                &mut sink,
                Arc::new(DenyApprover),
                Arc::new(NeverPause),
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("no session"), "unexpected: {err}");
        assert!(
            sink.events.is_empty(),
            "a rejected command must emit nothing"
        );

        let mut sink = CollectingSink::default();
        let err = service
            .run_agent_with_controls(&bob, session, "anything", "go", &mut sink)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("no session"), "unexpected: {err}");
        assert!(sink.events.is_empty(), "a rejected agent must emit nothing");

        // The owner gets past the check and fails later, for an unrelated reason. That
        // difference is what proves the authorization runs first.
        let mut sink = CollectingSink::default();
        let owner_err = service
            .run_agent_with_controls(&alice, session, "anything", "go", &mut sink)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            !owner_err.contains("no session"),
            "the owner must clear authorization, not be rejected by it: {owner_err}"
        );
    }

    /// export_promotion is machine-to-machine: it derives the owner itself and takes no
    /// principal, so there is no tautological "check" to pass.
    #[tokio::test]
    async fn export_promotion_needs_no_principal() {
        let (service, _tmp) = build_service_for_test().await;
        let alice = otto_protocol::UserId::parse("alice").unwrap();
        let session = service
            .create_session(&alice, "g", &serde_json::json!({}))
            .await
            .unwrap();
        let bundle = service.export_promotion(session).await.unwrap();
        assert_eq!(bundle.session.owner, alice);
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
            .create_session(&local(), "g", &serde_json::json!({}))
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
                owner: local(),
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
        assert!(service.store().session_status(&local(), id).await.is_ok());
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
                owner: local(),
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
        assert!(service.store().session_status(&local(), id).await.is_err());
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
                owner: local(),
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
        assert!(service.store().session_status(&local(), id).await.is_err());
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
        assert!(service.store().session_status(&local(), id).await.is_err());
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
        assert!(service.store().session_status(&local(), id).await.is_err());
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
        assert!(service.store().session_status(&local(), id).await.is_err());
    }

    #[tokio::test]
    async fn accept_promotion_duplicate_session_is_already_exists() {
        use otto_engine_core::types::WorkspaceSnapshot;
        use otto_remote::PromoteBundle;

        let ws_dir = tempfile::tempdir().unwrap();
        let db_dir = tempfile::tempdir().unwrap();
        let service = build_test_service(ws_dir.path(), db_dir.path().join("r.db")).await;

        let id = service
            .create_session(&local(), "g", &serde_json::json!({}))
            .await
            .unwrap();
        let state = service.store().snapshot(&local(), id).await.unwrap();
        let bundle = PromoteBundle {
            session: state,
            workspace: WorkspaceSnapshot { files: vec![] },
        };

        assert!(matches!(
            service.accept_promotion(&bundle).await,
            Err(AcceptError::AlreadyExists)
        ));
    }

    /// `restore_over` is an unconditional overwrite and the bundle is whatever the remote chose
    /// to return, so without the id binding a receiver could answer an export for session X with
    /// a bundle for session Y and overwrite Y — including its `owner`. This is the regression
    /// test for that binding: delete the check in `accept_demotion` and this goes red.
    #[tokio::test]
    async fn accept_demotion_refuses_a_bundle_for_a_different_session() {
        use otto_engine_core::types::WorkspaceSnapshot;
        use otto_persistence::SessionState;
        use otto_remote::PromoteBundle;

        let ws = tempfile::tempdir().unwrap();
        let db = tempfile::tempdir().unwrap();
        let service = build_test_service(ws.path(), db.path().join("s.db")).await;

        let victim = service
            .create_session(&local(), "victim", &serde_json::json!({}))
            .await
            .unwrap();
        let requested = otto_protocol::SessionId::new();

        // A well-formed bundle — but for the victim, not for the session we asked to demote.
        let bundle = PromoteBundle {
            session: SessionState {
                id: victim,
                owner: local(),
                goal: "attacker supplied".to_string(),
                status: otto_persistence::SessionStatus::Done,
                config: serde_json::json!({}),
                events: vec![],
                turns: vec![],
            },
            workspace: WorkspaceSnapshot { files: vec![] },
        };

        let err = service
            .accept_demotion(requested, &bundle)
            .await
            .expect_err("a bundle for a different session must be refused");
        assert!(
            format!("{err:?}").contains("was requested"),
            "unexpected: {err:?}"
        );
        // ...and the victim's row is untouched.
        assert_eq!(
            service
                .store()
                .snapshot(&local(), victim)
                .await
                .unwrap()
                .goal,
            "victim"
        );
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
            .create_session(&local(), "old", &serde_json::json!({}))
            .await
            .unwrap();

        // A bundle for the SAME id carrying advanced state + a new workspace file.
        let bundle = PromoteBundle {
            session: SessionState {
                id,
                owner: local(),
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
        let restored = service
            .accept_demotion(bundle.session.id, &bundle)
            .await
            .unwrap();
        assert_eq!(restored, id);
        assert_eq!(
            service.store().snapshot(&local(), id).await.unwrap().goal,
            "advanced"
        );
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

        // Seed a local row: the owner check runs before the sensitive-path validation, so the
        // demoted bundle must match a real local row for the workspace validation to be reached.
        let id = service
            .create_session(&local(), "seed", &serde_json::json!({}))
            .await
            .unwrap();
        let bundle = bundle_with(id, std::path::PathBuf::from(".env"), b"SECRET=1".to_vec());
        assert!(matches!(
            service.accept_demotion(bundle.session.id, &bundle).await,
            Err(crate::service::AcceptError::Refused(_))
        ));
        // Fail-closed: nothing landed — neither the file nor a session overwrite.
        assert!(
            service
                .workspace()
                .read(std::path::Path::new(".env"))
                .await
                .is_err()
        );
        assert_eq!(
            service.store().snapshot(&local(), id).await.unwrap().goal,
            "seed"
        );
    }

    /// `restore_over` overwrites the row INCLUDING its `owner`, so without this check a bundle
    /// carrying a different owner than the local row's current owner would reassign the local
    /// row's ownership. This is the second half of the overwrite-including-owner hole the
    /// id-binding (#123) started on: delete the owner check in `accept_demotion` and this goes red.
    #[tokio::test]
    async fn accept_demotion_refuses_a_bundle_with_a_different_owner() {
        use otto_engine_core::types::WorkspaceSnapshot;
        use otto_persistence::SessionState;
        use otto_remote::PromoteBundle;

        let ws = tempfile::tempdir().unwrap();
        let db = tempfile::tempdir().unwrap();
        let service = build_test_service(ws.path(), db.path().join("s.db")).await;

        let alice = otto_protocol::UserId::parse("alice").unwrap();
        let bob = otto_protocol::UserId::parse("bob").unwrap();
        let id = service
            .create_session(&alice, "alice's", &serde_json::json!({}))
            .await
            .unwrap();

        // Same id (the id binding passes) — but the bundle claims a different owner.
        let bundle = PromoteBundle {
            session: SessionState {
                id,
                owner: bob,
                goal: "bob's copy".to_string(),
                status: otto_persistence::SessionStatus::Done,
                config: serde_json::json!({}),
                events: vec![],
                turns: vec![],
            },
            workspace: WorkspaceSnapshot { files: vec![] },
        };

        let err = service
            .accept_demotion(id, &bundle)
            .await
            .expect_err("a bundle owned by someone other than the local row must be refused");
        assert!(matches!(err, AcceptError::Refused(_)));
        // The local row's owner is untouched — the overwrite never happened.
        assert_eq!(service.store().owner_of(id).await.unwrap(), alice);
    }

    #[tokio::test]
    async fn accept_demotion_accepts_a_bundle_with_the_same_owner() {
        use otto_engine_core::types::WorkspaceSnapshot;
        use otto_persistence::SessionState;
        use otto_remote::PromoteBundle;

        let ws = tempfile::tempdir().unwrap();
        let db = tempfile::tempdir().unwrap();
        let service = build_test_service(ws.path(), db.path().join("s.db")).await;

        let alice = otto_protocol::UserId::parse("alice").unwrap();
        let id = service
            .create_session(&alice, "alice's", &serde_json::json!({}))
            .await
            .unwrap();

        // Same id, same owner: a legitimate demote-pull. It overwrites and keeps the owner.
        let bundle = PromoteBundle {
            session: SessionState {
                id,
                owner: alice.clone(),
                goal: "advanced".to_string(),
                status: otto_persistence::SessionStatus::Done,
                config: serde_json::json!({}),
                events: vec![],
                turns: vec![],
            },
            workspace: WorkspaceSnapshot { files: vec![] },
        };

        let restored = service.accept_demotion(id, &bundle).await.unwrap();
        assert_eq!(restored, id);
        assert_eq!(service.store().owner_of(id).await.unwrap(), alice);
    }

    /// The owner lookup (`owner_of`) is a reverse-existence oracle: a demotion for an id with no
    /// local row must fail closed rather than restore a row that was never there. The lookup
    /// error is a genuine store-side failure, mapped to `Failed` (never leaked to a client).
    #[tokio::test]
    async fn accept_demotion_refuses_an_unknown_local_row() {
        use otto_engine_core::types::WorkspaceSnapshot;
        use otto_persistence::SessionState;
        use otto_remote::PromoteBundle;

        let ws = tempfile::tempdir().unwrap();
        let db = tempfile::tempdir().unwrap();
        let service = build_test_service(ws.path(), db.path().join("s.db")).await;

        let id = SessionId::new();
        let bundle = PromoteBundle {
            session: SessionState {
                id,
                owner: local(),
                goal: "g".to_string(),
                status: otto_persistence::SessionStatus::Done,
                config: serde_json::json!({}),
                events: vec![],
                turns: vec![],
            },
            workspace: WorkspaceSnapshot { files: vec![] },
        };

        let err = service.accept_demotion(id, &bundle).await;
        assert!(matches!(err, Err(AcceptError::Failed(_))));
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
            .create_session(&local(), "add a greeting", &serde_json::json!({}))
            .await
            .unwrap();
        let mut sink = CollectingSink::default();
        service
            .run_prompt(&local(), id, "add a greeting", &mut sink)
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

    #[tokio::test]
    async fn run_command_with_controls_unknown_name_errors_without_starting_a_turn() {
        let dir = tempfile::tempdir().unwrap();
        let service = service_in(&dir, crate::build_default_registry())
            .await
            .with_extensions(Arc::new(Extensions::default()));
        let id = service
            .create_session(&local(), "g", &serde_json::json!({}))
            .await
            .unwrap();

        let mut sink = CollectingSink::default();
        let err = service
            .run_command_with_controls(
                &local(),
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

        let replayed = service
            .store()
            .replay_since(&local(), id, None)
            .await
            .unwrap();
        assert!(replayed.is_empty(), "no turn should have started");
    }

    #[tokio::test]
    async fn run_command_with_controls_expands_args() {
        let dir = tempfile::tempdir().unwrap();
        let def = command_def("greet", "do $1", None);
        let extensions = Extensions {
            commands: vec![def],
            ..Default::default()
        };
        let service = service_in(&dir, crate::build_default_registry())
            .await
            .with_extensions(Arc::new(extensions));
        let id = service
            .create_session(&local(), "g", &serde_json::json!({}))
            .await
            .unwrap();

        let mut sink = CollectingSink::default();
        let outcome = service
            .run_command_with_controls(
                &local(),
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
        let state = service.store().snapshot(&local(), id).await.unwrap();
        assert_eq!(state.turns.last().unwrap().goal, "do thing");
    }

    #[tokio::test]
    async fn run_command_with_controls_narrows_tools_denies_excluded_injection() {
        let dir = tempfile::tempdir().unwrap();
        // The referenced file actually exists and is readable — so if `fs.read` were present in
        // the narrowed registry, the injection would resolve successfully and the turn would
        // proceed. That is what makes the assertion below load-bearing: the failure below can
        // only be explained by the narrowing itself, not by a missing/unreadable file.
        std::fs::write(dir.path().join("plain.txt"), b"CONTENT").unwrap();
        // allowed_tools excludes fs.read — the @$1 injection must be denied because the
        // NARROWED registry has no fs.read tool at all (ToolRegistry::call errors on a tool
        // absent from its map, independent of the shared gate's decision on the path).
        let def = command_def("narrow", "read @$1", Some(vec!["bash".to_string()]));
        let extensions = Extensions {
            commands: vec![def],
            ..Default::default()
        };
        let service = service_in(&dir, crate::build_default_registry())
            .await
            .with_extensions(Arc::new(extensions));
        let id = service
            .create_session(&local(), "g", &serde_json::json!({}))
            .await
            .unwrap();

        let mut sink = CollectingSink::default();
        let err = service
            .run_command_with_controls(
                &local(),
                id,
                "narrow",
                &["plain.txt".to_string()],
                &mut sink,
                Arc::new(DenyApprover),
                Arc::new(NeverPause),
            )
            .await
            .unwrap_err();
        // Proves BOTH expansion (the path in the error is the substituted "plain.txt", not
        // literal "$1") and narrowing (fs.read was excluded, so the injection is denied even
        // though plain.txt is not a sensitive path and the shared gate would otherwise allow it).
        assert!(
            err.to_string().contains("plain.txt"),
            "expected the expanded path in the error, got: {err}"
        );

        let replayed = service
            .store()
            .replay_since(&local(), id, None)
            .await
            .unwrap();
        assert!(replayed.is_empty(), "no turn should have started");
    }

    #[tokio::test]
    async fn run_command_with_controls_no_extensions_configured_errors() {
        let dir = tempfile::tempdir().unwrap();
        // No `.with_extensions(...)` call at all — `self.extensions` stays `None`.
        let service = service_in(&dir, crate::build_default_registry()).await;
        let id = service
            .create_session(&local(), "g", &serde_json::json!({}))
            .await
            .unwrap();

        let mut sink = CollectingSink::default();
        let err = service
            .run_command_with_controls(
                &local(),
                id,
                "anything",
                &[],
                &mut sink,
                Arc::new(DenyApprover),
                Arc::new(NeverPause),
            )
            .await
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("not configured with any extensions")
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
            .create_session(&local(), "g", &serde_json::json!({}))
            .await
            .unwrap();

        let mut sink = CollectingSink::default();
        let result = service
            .run_command_with_controls(
                &local(),
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

        let replayed = service
            .store()
            .replay_since(&local(), id, None)
            .await
            .unwrap();
        assert!(replayed.is_empty(), "no turn should have started");
    }

    #[tokio::test]
    async fn run_agent_with_controls_unknown_name_errors_without_starting_a_turn() {
        let dir = tempfile::tempdir().unwrap();
        let service = service_in(&dir, crate::build_default_registry())
            .await
            .with_extensions(Arc::new(Extensions::default()));
        let id = service
            .create_session(&local(), "g", &serde_json::json!({}))
            .await
            .unwrap();

        let mut sink = CollectingSink::default();
        let err = service
            .run_agent_with_controls(&local(), id, "ghost", "do it", &mut sink)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no custom agent named 'ghost'"));

        let replayed = service
            .store()
            .replay_since(&local(), id, None)
            .await
            .unwrap();
        assert!(replayed.is_empty(), "no turn should have started");
    }

    #[tokio::test]
    async fn run_agent_with_controls_no_extensions_configured_errors() {
        let dir = tempfile::tempdir().unwrap();
        // No `.with_extensions(...)` call at all — `self.extensions` stays `None`.
        let service = service_in(&dir, crate::build_default_registry()).await;
        let id = service
            .create_session(&local(), "g", &serde_json::json!({}))
            .await
            .unwrap();

        let mut sink = CollectingSink::default();
        let err = service
            .run_agent_with_controls(&local(), id, "anything", "do it", &mut sink)
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
        let def = agent_def(
            "reviewer",
            "SYSTEM-PROMPT",
            Some(vec!["fs.read".to_string()]),
            None,
        );
        let extensions = Extensions {
            agents: vec![def],
            ..Default::default()
        };
        let service = service_in(&dir, crate::build_default_registry())
            .await
            .with_extensions(Arc::new(extensions));
        let id = service
            .create_session(&local(), "g", &serde_json::json!({}))
            .await
            .unwrap();

        let mut sink = CollectingSink::default();
        let outcome = service
            .run_agent_with_controls(&local(), id, "reviewer", "look at auth.rs", &mut sink)
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
        let replayed = service
            .store()
            .replay_since(&local(), id, None)
            .await
            .unwrap();
        assert_eq!(replayed, sink.events);

        assert_eq!(
            service.store().session_status(&local(), id).await.unwrap(),
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
            .create_session(&local(), "g", &serde_json::json!({}))
            .await
            .unwrap();

        let mut sink = CollectingSink::default();
        let outcome = service
            .run_agent_with_controls(&local(), id, "second", "ping", &mut sink)
            .await
            .unwrap();
        assert!(outcome.ok);
        match &sink.events[1].kind {
            EventKind::Log { message } => assert!(message.contains("SECOND-PROMPT")),
            other => panic!("expected Log, got {other:?}"),
        }
    }

    /// A `Router` whose `complete` always fails — simulates a provider error (e.g. a remote
    /// LLM call failing) so the dispatch-failure branch of `run_agent_dispatch` can be exercised
    /// deterministically and offline. `run_agent_with_controls` itself always builds its router
    /// from the environment (never falls back to `self.router`), so this is wired in through
    /// `run_agent_with_controls_inner`'s test-only router-override parameter.
    struct FailingRouter;
    #[async_trait]
    impl Router for FailingRouter {
        async fn complete(
            &self,
            _req: otto_engine_core::types::CompleteRequest,
            _hints: otto_engine_core::router::RouteHints,
        ) -> anyhow::Result<otto_engine_core::types::CompleteResponse> {
            anyhow::bail!("simulated router failure")
        }
    }

    #[tokio::test]
    async fn run_agent_with_controls_dispatch_failure_marks_session_failed() {
        let dir = tempfile::tempdir().unwrap();
        let def = agent_def("reviewer", "SYSTEM-PROMPT", None, None);
        let extensions = Extensions {
            agents: vec![def],
            ..Default::default()
        };
        let service = service_in(&dir, crate::build_default_registry())
            .await
            .with_extensions(Arc::new(extensions));
        let id = service
            .create_session(&local(), "g", &serde_json::json!({}))
            .await
            .unwrap();

        let mut sink = CollectingSink::default();
        let outcome = service
            .run_agent_with_controls_inner(
                id,
                "reviewer",
                "look at auth.rs",
                &mut sink,
                Some(Arc::new(FailingRouter)),
            )
            .await
            .unwrap();
        // A dispatch failure is reported in-band as Ok(TurnOutcome { ok: false }), not Err — so
        // serve.rs's report_turn_outcome never sends a redundant Error frame after the
        // TurnComplete{ok:false} that already streamed for this same failure.
        assert!(!outcome.ok);

        // Fixed event sequence on a dispatch failure: AgentStarted, Log (carrying the error
        // text), AgentFinished, TurnComplete { ok: false }.
        assert_eq!(sink.events.len(), 4);
        assert!(matches!(
            sink.events[0].kind,
            EventKind::AgentStarted {
                role: Role::Custom(ref n)
            } if n == "reviewer"
        ));
        match &sink.events[1].kind {
            EventKind::Log { message } => assert!(message.contains("simulated router failure")),
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
            EventKind::TurnComplete { ok: false }
        ));

        // Persisted log matches what was streamed.
        let replayed = service
            .store()
            .replay_since(&local(), id, None)
            .await
            .unwrap();
        assert_eq!(replayed, sink.events);

        assert_eq!(
            service.store().session_status(&local(), id).await.unwrap(),
            SessionStatus::Failed
        );
    }

    #[tokio::test]
    async fn create_persists_active_session() {
        let dir = tempfile::tempdir().unwrap();
        let service = service_in(&dir, crate::build_default_registry()).await;
        let id = service
            .create_session(&local(), "do a thing", &serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(
            service.store().session_status(&local(), id).await.unwrap(),
            SessionStatus::Active
        );
    }

    #[tokio::test]
    async fn abort_sets_status_aborted() {
        let dir = tempfile::tempdir().unwrap();
        let service = service_in(&dir, crate::build_default_registry()).await;
        let id = service
            .create_session(&local(), "g", &serde_json::json!({}))
            .await
            .unwrap();
        service.abort(&local(), id).await.unwrap();
        assert_eq!(
            service.store().session_status(&local(), id).await.unwrap(),
            SessionStatus::Aborted
        );
    }

    #[tokio::test]
    async fn run_prompt_streams_persists_and_marks_done() {
        let dir = tempfile::tempdir().unwrap();
        let service = service_in(&dir, crate::build_default_registry()).await;
        let id = service
            .create_session(&local(), "add a greeting", &serde_json::json!({}))
            .await
            .unwrap();
        let mut sink = CollectingSink::default();
        let outcome = service
            .run_prompt(&local(), id, "add a greeting", &mut sink)
            .await
            .unwrap();

        assert!(outcome.ok);
        assert_eq!(
            service.store().session_status(&local(), id).await.unwrap(),
            SessionStatus::Done
        );
        // The streamed events equal the persisted log, with contiguous seqs from 0.
        let replayed = service
            .store()
            .replay_since(&local(), id, None)
            .await
            .unwrap();
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
            .create_session(&local(), "g", &serde_json::json!({}))
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
            .run_prompt_with_controls(&local(), id, "g", &mut sink, controls)
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
            .create_session(&local(), "g", &serde_json::json!({}))
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
            .run_prompt_with_controls(&local(), id, "g", &mut sink, controls)
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
            .create_session(&local(), "g", &serde_json::json!({}))
            .await
            .unwrap();

        let mut s1 = CollectingSink::default();
        service
            .run_prompt(&local(), id, "g", &mut s1)
            .await
            .unwrap();
        let mut s2 = CollectingSink::default();
        service
            .run_prompt(&local(), id, "g", &mut s2)
            .await
            .unwrap();

        let last1 = s1.events.last().unwrap().seq;
        assert_eq!(s2.events.first().unwrap().seq, last1 + 1);

        let all = service
            .store()
            .replay_since(&local(), id, None)
            .await
            .unwrap();
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
            .create_session(&local(), "g", &serde_json::json!({}))
            .await
            .unwrap();
        let mut sink = CollectingSink::default();
        let result = service.run_prompt(&local(), id, "g", &mut sink).await;
        assert!(result.is_err());
        assert_eq!(
            service.store().session_status(&local(), id).await.unwrap(),
            SessionStatus::Failed
        );
    }

    #[test]
    fn history_from_records_parses_outcomes_and_skips_unparseable_ones() {
        use std::path::PathBuf;

        let records = vec![
            TurnRecord {
                turn_index: 0,
                goal: "first".to_string(),
                outcome: serde_json::json!({
                    "ok": true,
                    "milestones": ["m1"],
                    "files_edited": ["a.rs"],
                    "verify": { "ok": true, "detail": "passed" }
                }),
            },
            // A row written before TurnOutcome grew: must still load, with defaults.
            TurnRecord {
                turn_index: 1,
                goal: "legacy".to_string(),
                outcome: serde_json::json!({ "ok": false }),
            },
            // Corrupt: must be skipped, never panic.
            TurnRecord {
                turn_index: 2,
                goal: "bad".to_string(),
                outcome: serde_json::json!("not an object"),
            },
        ];

        let h = history_from_records(records);
        assert_eq!(h.turns().len(), 2);
        assert_eq!(h.turns()[0].milestones, vec!["m1".to_string()]);
        assert_eq!(h.turns()[0].files_edited, vec![PathBuf::from("a.rs")]);
        assert_eq!(h.turns()[1].goal, "legacy");
        assert!(h.turns()[1].milestones.is_empty());
        assert!(!h.turns()[1].ok);
    }
}
