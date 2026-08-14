//! `EmbeddedTransport` — the engine running in this process, behind the protocol seam.
//!
//! This is the one module in the crate that names engine types: it *is* the adapter between the
//! `ClientTransport` seam and an in-process `EngineService`, so it necessarily reaches inward.
//! Everything else in `otto-cli` (`transport.rs`, `render.rs`, the REPL) stays protocol-only, and
//! that boundary is what keeps the REPL honest — it cannot tell an embedded engine from a remote
//! one. No socket, no port, no auth, no sidecar process: this is what makes `cd repo && otto`
//! work with zero configuration.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use async_trait::async_trait;
use otto_engine::{
    EngineService, EventSink, McpConnection, TurnControls, build_capabilities,
    build_composed_tools, build_default_registry, build_retriever, build_router,
    preflight_base_urls, session_config,
};
use otto_engine_core::Router;
use otto_persistence::SqliteStore;
use otto_protocol::{Command, Event, EventKind, ServerMessage, SessionId, UserId};
use otto_workspace::LocalWorkspace;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::transport::ClientTransport;

/// An `EventSink` that forwards each persisted event straight onto the transport's outbound
/// queue. The engine-side counterpart of `otto_engine::CollectingSink`, which gathers into a
/// `Vec` instead; the REPL needs them live, one frame at a time.
struct ChannelSink {
    tx: UnboundedSender<ServerMessage>,
    /// Set by `Abort`. The engine has no cooperative cancellation, so an aborted turn keeps
    /// running to completion; silencing its sink is what stops the client from seeing frames
    /// after the terminal frame `Abort` already sent it. Persistence is unaffected — the service
    /// writes each event to the store *before* calling `emit`, so the session log stays complete.
    silenced: Arc<AtomicBool>,
    /// The highest seq forwarded to the client so far, so a synthesized frame (the abort's
    /// terminal `TurnComplete`) can be given a seq that is monotonic on the wire.
    last_seq: Arc<AtomicU64>,
}

#[async_trait]
impl EventSink for ChannelSink {
    async fn emit(&mut self, event: &Event) -> anyhow::Result<()> {
        if self.silenced.load(Ordering::SeqCst) {
            return Ok(());
        }
        self.last_seq.fetch_max(event.seq, Ordering::SeqCst);
        // A closed receiver means the transport was dropped mid-turn; the turn is now pointless
        // but not *wrong*, so this is not an error the engine should fail the turn over.
        let _ = self.tx.send(ServerMessage::Event {
            event: event.clone(),
        });
        Ok(())
    }
}

/// The transport's handle on the turn it most recently started.
struct CurrentTurn {
    /// Which session the turn belongs to, so an `Abort` naming a different session cannot
    /// silence it.
    session: SessionId,
    silenced: Arc<AtomicBool>,
    /// Deliberately never cancelled — see the `Abort` arm for why `handle.abort()` would be
    /// actively harmful — so production genuinely never reads this. It is kept because a
    /// `JoinHandle` is the only way to wait for a turn to become *durable* (the service records
    /// the turn after emitting `TurnComplete`), which the test helper does. Not underscore-named
    /// like `_mcp`: dropping this handle detaches the task rather than keeping anything alive, so
    /// that spelling would claim a guarantee it does not provide.
    #[cfg_attr(not(test), allow(dead_code))]
    handle: tokio::task::JoinHandle<()>,
}

/// The engine, in this process, speaking the wire protocol.
pub struct EmbeddedTransport {
    service: Arc<EngineService>,
    owner: UserId,
    rx: UnboundedReceiver<ServerMessage>,
    tx: UnboundedSender<ServerMessage>,
    /// Kept alive for this transport's whole lifetime: these guards own the spawned MCP server
    /// child processes, and dropping them kills the children — so the fs/grep/git/lsp/bash and
    /// plugin tools would silently stop working mid-session. `cmd_run` gets away with a local
    /// binding because its whole turn happens inside one function; a transport outlives any
    /// single call, so the guards have to live on the struct.
    _mcp: Vec<McpConnection>,
    /// The most recently started turn, if any — so `Abort` can find it. The engine serializes
    /// turns behind its own lock, so a second `SendPrompt` sent before the first finishes simply
    /// queues; only the latest is tracked here, which is what a REPL (one prompt at a time) needs.
    current: Option<CurrentTurn>,
    /// Highest seq forwarded to the client, shared with every turn's sink.
    last_seq: Arc<AtomicU64>,
}

impl EmbeddedTransport {
    /// Build an engine over `root` with the ambient home directory (where `~/.claude` lives).
    pub async fn new(root: PathBuf) -> anyhow::Result<Self> {
        Self::new_in(root, dirs::home_dir().unwrap_or_default()).await
    }

    /// [`Self::new`] with the home directory injected, so callers never read the developer's real
    /// `~/.claude` (the same `_in` convention `otto run --agent`/`--command` already use).
    pub async fn new_in(root: PathBuf, home: PathBuf) -> anyhow::Result<Self> {
        Self::new_with_router(root, home, None).await
    }

    /// [`Self::new_in`] with the LLM router injected. `None` — the production path — builds the
    /// router from the environment via [`build_router`], exactly as `otto run` does; production
    /// behavior is unchanged by this seam existing.
    ///
    /// `Some(router)` exists because [`build_router`] selects a *real remote provider* whenever
    /// `ANTHROPIC_API_KEY`/`OPENAI_API_KEY`/`GEMINI_API_KEY`/`DEEPSEEK_API_KEY` is exported. A
    /// test that runs a turn through the ambient router is offline only by accident of the
    /// machine it runs on: with a key set it would hit the network, cost money, stop being
    /// deterministic, and let a real Coder apply gated edits. Pinning the router is how a caller
    /// makes "offline" a property of the code rather than of the environment.
    ///
    /// Everything else is the dependency graph `cmd_run` builds — the same
    /// `build_composed_tools` composition (permission gate, skills, plugin MCP servers, hooks)
    /// and the same retriever — so the CLI's permission, hook, skill, and plugin behavior cannot
    /// drift from the rest of the engine.
    pub async fn new_with_router(
        root: PathBuf,
        home: PathBuf,
        router: Option<Arc<dyn Router>>,
    ) -> anyhow::Result<Self> {
        // A bad *_BASE_URL degrades the library to the offline canned provider, which still
        // produces a complete-looking turn. In a CLI — where a human set the variable — refuse to
        // start instead, exactly as `otto run` does.
        preflight_base_urls()?;

        // The orchestrator and the tools get separate `LocalWorkspace` handles over the same root,
        // exactly as `cmd_run` does. Each `Arc::new` below is unsized to the corresponding trait
        // object at the call site, so `Workspace`/`SessionStore` never have to be named here.
        let orch_workspace = Arc::new(LocalWorkspace::new(root.clone()));
        let tools_workspace = Arc::new(LocalWorkspace::new(root.clone()));

        // Discover extensions first: the permission rules are needed at registry-construction
        // time so the gate can be a `PolicyGate`.
        let ext = otto_extensions::discover(&root, &home);
        let (tools, mcp) = build_composed_tools(&ext, tools_workspace, root.clone(), false).await;

        let db = session_db_path(&root);
        if let Some(parent) = db.parent() {
            // sqlite will not create intermediate directories; `.otto/` normally does not exist
            // yet on a repo's first `otto` run.
            std::fs::create_dir_all(parent)?;
        }
        let retriever = build_retriever(&root).await;
        let service = EngineService::new(
            Arc::new(SqliteStore::open(db).await?),
            Arc::new(build_default_registry()),
            router.unwrap_or_else(|| Arc::from(build_router())),
            orch_workspace,
            Arc::new(tools),
        )
        .with_retriever(retriever)
        .with_extensions(Arc::new(ext));

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        Ok(Self {
            service: Arc::new(service),
            owner: UserId::local(),
            rx,
            tx,
            _mcp: mcp,
            current: None,
            last_seq: Arc::new(AtomicU64::new(0)),
        })
    }

    /// Queue a frame for the client. The receiver is owned by `self`, so this cannot fail in
    /// practice; a closed channel simply drops the frame.
    fn push(&self, msg: ServerMessage) {
        let _ = self.tx.send(msg);
    }

    fn push_error(&self, message: impl Into<String>) {
        self.push(ServerMessage::Error {
            message: message.into(),
        });
    }

    /// How many turns this session has recorded.
    ///
    /// Takes `&mut self` and settles the in-flight turn first: the engine emits `TurnComplete`
    /// from inside the turn and only records the turn afterwards, so a caller that reads the
    /// store the instant it sees `TurnComplete` would race the write. Joining the turn task is
    /// the honest way to ask "and is it durable yet?".
    #[cfg(test)]
    pub(crate) async fn turn_count(&mut self, session: SessionId) -> anyhow::Result<usize> {
        if let Some(turn) = self.current.take() {
            let _ = turn.handle.await;
        }
        Ok(self
            .service
            .store()
            .turns(&self.owner, session)
            .await?
            .len())
    }

    /// The session's persisted status, after letting the in-flight turn finish.
    ///
    /// The wait is the point: an aborted turn is silenced, not cancelled, so it runs on to its own
    /// terminal status write. Reading before that write would pass whether or not the write
    /// respects the abort.
    #[cfg(test)]
    pub(crate) async fn session_status(
        &mut self,
        session: SessionId,
    ) -> anyhow::Result<otto_persistence::SessionStatus> {
        if let Some(turn) = self.current.take() {
            let _ = turn.handle.await;
        }
        self.service
            .store()
            .session_status(&self.owner, session)
            .await
    }
}

/// Where this root's session database lives: `OTTO_DB` when the operator names one (matching
/// `otto run`/`otto serve`), else a dot-directory inside the workspace.
///
/// The dot-prefix is load-bearing: `LocalWorkspace::list` skips dot-directories, so the session
/// store never ends up inside a `workspace.snapshot()` of the repo it is recording. Rooting it at
/// the workspace rather than the process CWD is what makes `cd repo && otto` resume *that repo's*
/// history no matter where it was launched from.
fn session_db_path(root: &std::path::Path) -> PathBuf {
    if let Some(explicit) = std::env::var_os("OTTO_DB") {
        return PathBuf::from(explicit);
    }
    root.join(".otto").join("sessions.db")
}

#[async_trait]
impl ClientTransport for EmbeddedTransport {
    async fn send(&mut self, cmd: Command) -> anyhow::Result<()> {
        match cmd {
            Command::CreateSession => {
                match self
                    .service
                    .create_session(&self.owner, "", &session_config())
                    .await
                {
                    // `Ready` is the protocol's session-established frame — there is no
                    // `SessionCreated`. `serve.rs` sends the same frame after its handshake, and
                    // computes its manifest the same way: `CapabilitiesManifest::default()` would
                    // report every capability as false, which is not merely uninformative but
                    // wrong on a machine that has a sandbox or a configured provider.
                    Ok(session) => self.push(ServerMessage::Ready {
                        session,
                        capabilities: build_capabilities(),
                    }),
                    Err(e) => self.push_error(e.to_string()),
                }
            }
            Command::SendPrompt { session, text } => {
                let service = Arc::clone(&self.service);
                let owner = self.owner.clone();
                let tx = self.tx.clone();
                let silenced = Arc::new(AtomicBool::new(false));
                let handle = tokio::spawn({
                    let silenced = Arc::clone(&silenced);
                    let last_seq = Arc::clone(&self.last_seq);
                    async move {
                        let mut sink = ChannelSink {
                            tx: tx.clone(),
                            silenced: Arc::clone(&silenced),
                            last_seq,
                        };
                        // Approval stays deny-only in this slice: `TurnControls::default()`
                        // carries `DenyApprover`, so an `Ask` edit is logged and skipped rather
                        // than applied. Interactive diff review is the next slice.
                        let controls = TurnControls::default();
                        if let Err(e) = service
                            .run_prompt_with_controls(&owner, session, &text, &mut sink, controls)
                            .await
                        {
                            // An aborted turn already got its terminal frame; do not follow it
                            // with a stray diagnostic.
                            if !silenced.load(Ordering::SeqCst) {
                                let _ = tx.send(ServerMessage::Error {
                                    message: e.to_string(),
                                });
                            }
                        }
                    }
                });
                self.current = Some(CurrentTurn {
                    session,
                    silenced,
                    handle,
                });
            }
            Command::Abort { session } => {
                if let Err(e) = self.service.abort(&self.owner, session).await {
                    self.push_error(e.to_string());
                }
                // Silence the in-flight turn for THIS session rather than cancelling its task.
                //
                // `EngineService::abort` only writes `SessionStatus::Aborted`; it does not signal
                // the running turn, and the engine has no cooperative cancellation. Cancelling the
                // task would drop the `run_prompt_with_controls` future, which (a) detaches the
                // inner orchestrator task — dropping a tokio `JoinHandle` does not stop it, so it
                // would keep applying gated `fs.write` edits anyway — and (b) releases the
                // service's turn lock, letting the next `SendPrompt` start a *second* orchestrator
                // against the same workspace. Silencing buys the honest half of that (the client
                // stops hearing from the turn) without the dishonest half.
                if let Some(turn) = self.current.as_ref().filter(|t| t.session == session) {
                    turn.silenced.store(true, Ordering::SeqCst);
                }
                // Always answer, turn or no turn. The transport owns both ends of the channel, so
                // `recv()` never yields `None`: without a terminal frame here a client sitting in
                // `recv()` after an abort waits forever, with no output and no diagnostic.
                // Synthesized, never persisted — the seq only has to be monotonic on the wire, and
                // the silenced turn will not emit a competing `TurnComplete`.
                let seq = self.last_seq.fetch_add(1, Ordering::SeqCst) + 1;
                self.push(ServerMessage::Event {
                    event: Event {
                        seq,
                        session,
                        kind: EventKind::TurnComplete { ok: false },
                    },
                });
            }
            // Accepted and ignored: the approver denies, so there is nothing to approve yet.
            // Answering with an error would train the REPL to treat approval as unsupported.
            Command::ApproveDiff { .. } => {}
            // Everything else — `RunCommand`/`RunAgent`, pause/resume, the handover pair, and
            // the whole auth family — is either a later slice or meaningless without a server.
            // An unsupported command is answered, never a panic: a REPL that can crash its own
            // engine by typing the wrong thing is worse than one that says no.
            other => self.push_error(format!(
                "{other:?} is not supported by the embedded transport"
            )),
        }
        Ok(())
    }

    async fn recv(&mut self) -> Option<ServerMessage> {
        self.rx.recv().await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use otto_protocol::{Command, EventKind, ServerMessage, SessionId};

    use super::EmbeddedTransport;
    use crate::transport::ClientTransport;

    /// A transport whose LLM router is pinned to the offline, deterministic
    /// `SingleProviderRouter(LocalProvider)` — byte-for-byte what `build_router()` builds when no
    /// provider key is exported.
    ///
    /// Every test uses this rather than `new`/`new_in`. Going through the ambient router would
    /// make these tests offline only by accident of the machine: with `ANTHROPIC_API_KEY` (or any
    /// of the other three) exported, the two turn tests would reach the network, cost money, stop
    /// being deterministic, and hand a real Coder a gated `fs.write`. The home directory is
    /// injected for the same reason — otherwise `otto_extensions::discover` would pick up the
    /// developer's real `~/.claude` hooks, skills, permissions, and plugin MCP servers.
    async fn offline_transport(
        root: &std::path::Path,
        home: &std::path::Path,
    ) -> EmbeddedTransport {
        let router = Arc::new(otto_router::SingleProviderRouter::new(Arc::new(
            otto_providers::LocalProvider::new(),
        )));
        EmbeddedTransport::new_with_router(root.to_path_buf(), home.to_path_buf(), Some(router))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn embedded_transport_runs_a_turn_and_streams_events() {
        let dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello").unwrap();

        let mut t = offline_transport(dir.path(), home.path()).await;
        t.send(Command::CreateSession).await.unwrap();
        let session = match t.recv().await {
            Some(ServerMessage::Ready { session, .. }) => session,
            other => panic!("expected Ready, got {other:?}"),
        };

        t.send(Command::SendPrompt {
            session,
            text: "add a hello function".to_string(),
        })
        .await
        .unwrap();

        let mut saw_turn_complete = false;
        while let Some(msg) = t.recv().await {
            match msg {
                ServerMessage::Event { event } => {
                    if matches!(event.kind, EventKind::TurnComplete { .. }) {
                        saw_turn_complete = true;
                        break;
                    }
                }
                // Without this arm a failed turn would leave the loop waiting forever on a
                // channel whose only other writer is finished: a hung test, not a failed one.
                ServerMessage::Error { message } => panic!("turn failed: {message}"),
                other => panic!("unexpected frame during a turn: {other:?}"),
            }
        }
        assert!(
            saw_turn_complete,
            "a turn must stream through to TurnComplete"
        );
    }

    #[tokio::test]
    async fn embedded_transport_carries_history_between_turns() {
        let dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let mut t = offline_transport(dir.path(), home.path()).await;
        t.send(Command::CreateSession).await.unwrap();
        let session = match t.recv().await {
            Some(ServerMessage::Ready { session, .. }) => session,
            other => panic!("expected Ready, got {other:?}"),
        };

        for text in ["first goal", "second goal"] {
            t.send(Command::SendPrompt {
                session,
                text: text.to_string(),
            })
            .await
            .unwrap();
            while let Some(ServerMessage::Event { event }) = t.recv().await {
                if matches!(event.kind, EventKind::TurnComplete { .. }) {
                    break;
                }
            }
        }

        // The second turn must have seen the first: assert on the store, not on model output,
        // so this stays deterministic offline.
        assert_eq!(t.turn_count(session).await.unwrap(), 2);
    }

    /// `Abort` must ALWAYS produce a terminal frame, including when no turn is running.
    ///
    /// The transport owns both ends of its channel, so `recv()` never yields `None`. If the
    /// `Abort` arm pushed nothing, a client sitting in `recv()` — which is exactly what the REPL
    /// loop does — would wait forever with no output and no diagnostic. This variant is the
    /// race-free one: with no turn in flight there is nothing else that could have queued a frame,
    /// so the frame observed here can only have come from the abort itself.
    #[tokio::test]
    async fn abort_without_a_turn_still_yields_a_terminal_frame() {
        let dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let mut t = offline_transport(dir.path(), home.path()).await;

        t.send(Command::CreateSession).await.unwrap();
        let session = match t.recv().await {
            Some(ServerMessage::Ready { session, .. }) => session,
            other => panic!("expected Ready, got {other:?}"),
        };

        t.send(Command::Abort { session }).await.unwrap();

        match t.recv().await {
            Some(ServerMessage::Event { event }) => {
                assert_eq!(event.session, session);
                assert!(
                    matches!(event.kind, EventKind::TurnComplete { ok: false }),
                    "an abort must terminate the turn, and not as a success: {:?}",
                    event.kind
                );
            }
            other => panic!("expected a terminal TurnComplete frame, got {other:?}"),
        }
    }

    /// The same guarantee with a turn actually in flight: the client is never left waiting.
    /// Race-free because it drains whatever the turn had already emitted and only requires that a
    /// `TurnComplete` arrives — the abort synthesizes one even if the turn is still running.
    #[tokio::test]
    async fn abort_during_a_turn_yields_a_terminal_frame() {
        let dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let mut t = offline_transport(dir.path(), home.path()).await;

        t.send(Command::CreateSession).await.unwrap();
        let session = match t.recv().await {
            Some(ServerMessage::Ready { session, .. }) => session,
            other => panic!("expected Ready, got {other:?}"),
        };

        t.send(Command::SendPrompt {
            session,
            text: "a long goal".to_string(),
        })
        .await
        .unwrap();
        t.send(Command::Abort { session }).await.unwrap();

        loop {
            match t.recv().await {
                Some(ServerMessage::Event { event }) => {
                    if matches!(event.kind, EventKind::TurnComplete { .. }) {
                        break;
                    }
                }
                other => panic!("expected events then a TurnComplete, got {other:?}"),
            }
        }
    }

    /// An abort must still read back as `Aborted` once the silenced turn has run to completion.
    ///
    /// This is the half `abort_during_a_turn_yields_a_terminal_frame` does not cover: that test
    /// asserts on the *frame*, so it stayed green while the turn's own terminal `set_status`
    /// overwrote the abort with `Done`/`Failed` in the store. Nothing in today's REPL reads the
    /// status, but `--continue`/`--resume` will.
    ///
    /// Race-free in both directions: if the abort lands first the engine's guard skips the turn's
    /// terminal write; if it lands after, the abort is simply the last writer. `Aborted` either way.
    #[tokio::test]
    async fn abort_survives_the_silenced_turn_running_to_completion() {
        let dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let mut t = offline_transport(dir.path(), home.path()).await;

        t.send(Command::CreateSession).await.unwrap();
        let session = match t.recv().await {
            Some(ServerMessage::Ready { session, .. }) => session,
            other => panic!("expected Ready, got {other:?}"),
        };

        t.send(Command::SendPrompt {
            session,
            text: "a goal to abandon".to_string(),
        })
        .await
        .unwrap();
        t.send(Command::Abort { session }).await.unwrap();

        assert_eq!(
            t.session_status(session).await.unwrap(),
            otto_persistence::SessionStatus::Aborted,
            "the completing turn must not overwrite the abort"
        );
    }

    /// An unsupported command must be answered, not fatal. `RunAgent` stands in for the whole
    /// class (pause/resume, handover, auth): the REPL can send anything, and the transport owes
    /// it a frame back rather than a panic.
    #[tokio::test]
    async fn unsupported_command_yields_an_error_frame_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let mut t = offline_transport(dir.path(), home.path()).await;

        t.send(Command::RunAgent {
            session: SessionId::new(),
            name: "reviewer".to_string(),
            prompt: "look".to_string(),
        })
        .await
        .unwrap();

        match t.recv().await {
            Some(ServerMessage::Error { message }) => {
                assert!(message.contains("not supported"), "got: {message}");
            }
            other => panic!("expected an Error frame, got {other:?}"),
        }
    }

    /// `ApproveDiff` is accepted silently in this slice — the approver denies, so there is
    /// nothing to approve. It must not error and must not panic.
    #[tokio::test]
    async fn approve_diff_is_accepted_without_a_reply() {
        let dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let mut t = offline_transport(dir.path(), home.path()).await;

        t.send(Command::ApproveDiff {
            session: SessionId::new(),
            id: uuid::Uuid::new_v4(),
            approved: true,
        })
        .await
        .unwrap();

        // Nothing queued: prove it without blocking forever on an empty channel.
        t.send(Command::CreateSession).await.unwrap();
        assert!(
            matches!(t.recv().await, Some(ServerMessage::Ready { .. })),
            "ApproveDiff must not have queued a frame ahead of the next command's reply"
        );
    }
}
