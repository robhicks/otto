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

use async_trait::async_trait;
use otto_engine::{
    EngineService, EventSink, McpConnection, TurnControls, build_composed_tools,
    build_default_registry, build_retriever, build_router, preflight_base_urls, session_config,
};
use otto_persistence::SqliteStore;
use otto_protocol::{CapabilitiesManifest, Command, Event, ServerMessage, UserId};
use otto_workspace::LocalWorkspace;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::transport::ClientTransport;

/// An `EventSink` that forwards each persisted event straight onto the transport's outbound
/// queue. The engine-side counterpart of `otto_engine::CollectingSink`, which gathers into a
/// `Vec` instead; the REPL needs them live, one frame at a time.
struct ChannelSink {
    tx: UnboundedSender<ServerMessage>,
}

#[async_trait]
impl EventSink for ChannelSink {
    async fn emit(&mut self, event: &Event) -> anyhow::Result<()> {
        // A closed receiver means the transport was dropped mid-turn; the turn is now pointless
        // but not *wrong*, so this is not an error the engine should fail the turn over.
        let _ = self.tx.send(ServerMessage::Event {
            event: event.clone(),
        });
        Ok(())
    }
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
    /// The most recently started turn, if any — so `Abort` can actually stop it. The engine
    /// serializes turns behind its own lock, so a second `SendPrompt` sent before the first
    /// finishes simply queues; only the latest is tracked here, which is what a REPL (one prompt
    /// at a time) needs.
    current: Option<tokio::task::JoinHandle<()>>,
}

impl EmbeddedTransport {
    /// Build an engine over `root` with the ambient home directory (where `~/.claude` lives).
    pub async fn new(root: PathBuf) -> anyhow::Result<Self> {
        Self::new_in(root, dirs::home_dir().unwrap_or_default()).await
    }

    /// [`Self::new`] with the home directory injected, so tests never read the developer's real
    /// `~/.claude` (the same `_in` convention `otto run --agent`/`--command` already use). All
    /// the wiring lives here.
    ///
    /// The dependency graph is the one `cmd_run` builds — the same router, the same
    /// `build_composed_tools` composition (permission gate, skills, plugin MCP servers, hooks),
    /// the same retriever — so the CLI's permission, hook, skill, and plugin behavior cannot
    /// drift from the rest of the engine.
    pub async fn new_in(root: PathBuf, home: PathBuf) -> anyhow::Result<Self> {
        // A bad *_BASE_URL degrades the library to the offline canned provider, which still
        // produces a complete-looking turn. In a CLI — where a human set the variable — refuse to
        // start instead, exactly as `otto run` does.
        preflight_base_urls()?;

        // The orchestrator and the tools get separate `LocalWorkspace` handles over the same root,
        // exactly as `cmd_run` does. Each `Arc::new` below is unsized to the corresponding trait
        // object at the call site, so this module never has to name an `engine-core` seam.
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
            Arc::from(build_router()),
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
    pub(crate) async fn turn_count(
        &mut self,
        session: otto_protocol::SessionId,
    ) -> anyhow::Result<usize> {
        if let Some(handle) = self.current.take() {
            let _ = handle.await;
        }
        Ok(self
            .service
            .store()
            .turns(&self.owner, session)
            .await?
            .len())
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
                    // `SessionCreated`. `serve.rs` sends the same frame after its handshake.
                    Ok(session) => self.push(ServerMessage::Ready {
                        session,
                        capabilities: CapabilitiesManifest::default(),
                    }),
                    Err(e) => self.push_error(e.to_string()),
                }
            }
            Command::SendPrompt { session, text } => {
                let service = Arc::clone(&self.service);
                let owner = self.owner.clone();
                let tx = self.tx.clone();
                self.current = Some(tokio::spawn(async move {
                    let mut sink = ChannelSink { tx: tx.clone() };
                    // Approval stays deny-only in this slice: `TurnControls::default()` carries
                    // `DenyApprover`, so an `Ask` edit is logged and skipped rather than applied.
                    // Interactive diff review is the next slice.
                    let controls = TurnControls::default();
                    if let Err(e) = service
                        .run_prompt_with_controls(&owner, session, &text, &mut sink, controls)
                        .await
                    {
                        let _ = tx.send(ServerMessage::Error {
                            message: e.to_string(),
                        });
                    }
                }));
            }
            Command::Abort { session } => {
                if let Err(e) = self.service.abort(&self.owner, session).await {
                    self.push_error(e.to_string());
                }
                if let Some(handle) = self.current.take() {
                    handle.abort();
                }
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
    use otto_protocol::{Command, EventKind, ServerMessage, SessionId};

    use super::EmbeddedTransport;
    use crate::transport::ClientTransport;

    #[tokio::test]
    async fn embedded_transport_runs_a_turn_and_streams_events() {
        let dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello").unwrap();

        let mut t = EmbeddedTransport::new_in(dir.path().to_path_buf(), home.path().to_path_buf())
            .await
            .unwrap();
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
        let mut t = EmbeddedTransport::new_in(dir.path().to_path_buf(), home.path().to_path_buf())
            .await
            .unwrap();
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

    /// An unsupported command must be answered, not fatal. `RunAgent` stands in for the whole
    /// class (pause/resume, handover, auth): the REPL can send anything, and the transport owes
    /// it a frame back rather than a panic.
    #[tokio::test]
    async fn unsupported_command_yields_an_error_frame_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let mut t = EmbeddedTransport::new_in(dir.path().to_path_buf(), home.path().to_path_buf())
            .await
            .unwrap();

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
        let mut t = EmbeddedTransport::new_in(dir.path().to_path_buf(), home.path().to_path_buf())
            .await
            .unwrap();

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
