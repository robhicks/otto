//! The engine as the REPL sees it: a `Command` sink and a `ServerMessage` stream, nothing more.

use async_trait::async_trait;
use otto_protocol::{Command, ServerMessage};

/// The engine as the REPL sees it: a `Command` sink and a `ServerMessage` stream, nothing more.
///
/// The REPL is written against this trait alone — it never names `EngineService`,
/// `Orchestrator`, or `ToolRegistry`. That keeps the CLI a genuine protocol client rather than
/// a privileged one, and it is what makes the loop testable without a live engine or a TTY.
#[async_trait]
pub trait ClientTransport: Send {
    async fn send(&mut self, cmd: Command) -> anyhow::Result<()>;
    /// The next message, or `None` when the engine is finished/disconnected.
    async fn recv(&mut self) -> Option<ServerMessage>;
}

/// A scripted `ClientTransport` for testing the REPL loop with no engine. Always compiled — the
/// same precedent as `providers::ScriptedProvider`.
pub struct FakeTransport {
    sent: Vec<Command>,
    scripted: std::collections::VecDeque<ServerMessage>,
}

impl FakeTransport {
    pub fn new(scripted: Vec<ServerMessage>) -> Self {
        Self {
            sent: Vec::new(),
            scripted: scripted.into(),
        }
    }

    pub fn sent(&self) -> &[Command] {
        &self.sent
    }
}

#[async_trait]
impl ClientTransport for FakeTransport {
    async fn send(&mut self, cmd: Command) -> anyhow::Result<()> {
        self.sent.push(cmd);
        Ok(())
    }

    async fn recv(&mut self) -> Option<ServerMessage> {
        self.scripted.pop_front()
    }
}

#[cfg(test)]
mod tests {
    use otto_protocol::{Command, Event, EventKind, ServerMessage, SessionId};

    use super::{ClientTransport, FakeTransport};

    #[tokio::test]
    async fn fake_transport_records_sends_and_replays_scripted_messages() {
        let session = SessionId::new();
        let mut t = FakeTransport::new(vec![ServerMessage::Event {
            event: Event {
                seq: 0,
                session,
                kind: EventKind::TurnComplete { ok: true },
            },
        }]);

        t.send(Command::SendPrompt {
            session,
            text: "hi".to_string(),
        })
        .await
        .unwrap();
        assert_eq!(t.sent().len(), 1);
        assert!(matches!(t.recv().await, Some(ServerMessage::Event { .. })));
        assert!(t.recv().await.is_none(), "exhausted script ends the stream");
    }
}
