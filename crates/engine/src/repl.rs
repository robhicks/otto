//! The interactive REPL: `cd repo && otto` opens a prompt with no configuration.
//!
//! `run_loop`/`create_session`/`run_one` are written against `Command`/`ServerMessage` and the
//! `otto_cli::ClientTransport` trait alone — they must never name `EngineService`,
//! `Orchestrator`, `ToolRegistry`, or `Workspace` directly. That is what lets `run_loop` be tested
//! against `otto_cli::FakeTransport` with no engine and no terminal, and it is what makes the loop
//! unable to tell an embedded engine from a remote one. Only the top-level `repl` entry point
//! (which constructs the concrete `EmbeddedTransport`) reaches into this crate's own wiring.
//!
//! This module lives in `otto-engine` rather than `otto-cli` (where the design was first laid
//! out) because `otto-cli` depending on `otto-engine` while `otto-engine`'s own `otto` binary
//! depends on `otto-cli` is a genuine Cargo package cycle — see `embedded.rs`'s module doc for the
//! full explanation.

use std::io::Write;

use otto_cli::ClientTransport;
use otto_protocol::{Command, EventKind, ServerMessage, SessionId};

/// Send `CreateSession` and wait for the session-established frame.
///
/// `Ready` is the protocol's session-established frame — there is no `SessionCreated` variant.
async fn create_session<T: ClientTransport>(transport: &mut T) -> anyhow::Result<SessionId> {
    transport.send(Command::CreateSession).await?;
    loop {
        match transport.recv().await {
            Some(ServerMessage::Ready { session, .. }) => return Ok(session),
            Some(ServerMessage::Error { message }) => anyhow::bail!("{message}"),
            Some(_) => continue,
            None => anyhow::bail!("engine closed before creating a session"),
        }
    }
}

/// Drive a single prompt to completion, rendering each event as it arrives.
///
/// A server-side `Error` is reported and ends the turn without ending the session — a failed
/// turn must return to the prompt, never exit the REPL.
async fn run_one<T: ClientTransport>(
    transport: &mut T,
    session: SessionId,
    text: String,
    out: &mut impl Write,
) -> anyhow::Result<()> {
    let color = otto_cli::render::color_enabled();
    transport
        .send(Command::SendPrompt { session, text })
        .await?;

    loop {
        match transport.recv().await {
            Some(ServerMessage::Event { event }) => {
                for line in otto_cli::render::render(&event.kind, color) {
                    writeln!(out, "{line}")?;
                }
                if matches!(event.kind, EventKind::TurnComplete { .. }) {
                    return Ok(());
                }
            }
            // Server-originated text, reproduced verbatim. A failed turn returns to the prompt;
            // it never ends the session.
            Some(ServerMessage::Error { message }) => {
                writeln!(out, "error: {message}")?;
                return Ok(());
            }
            Some(_) => continue,
            None => return Ok(()),
        }
    }
}

/// Drive one prompt per input item, rendering events until each turn completes.
///
/// Generic over the input iterator and the output sink so the loop can be tested with scripted
/// input and an in-memory buffer — no TTY, no PTY, no engine.
pub(crate) async fn run_loop<T: ClientTransport>(
    transport: &mut T,
    input: impl Iterator<Item = String>,
    out: &mut impl Write,
) -> anyhow::Result<()> {
    let session = create_session(transport).await?;

    for line in input {
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        run_one(transport, session, line, out).await?;
    }
    Ok(())
}

/// Start the interactive REPL against an in-process engine rooted at `root`.
pub async fn repl(root: std::path::PathBuf) -> anyhow::Result<()> {
    // Same fail-fast posture as `otto run`: a bad *_BASE_URL must not silently degrade to the
    // canned offline provider inside an interactive session.
    crate::preflight_base_urls()?;

    let mut transport = crate::embedded::EmbeddedTransport::new(root).await?;
    let mut stdout = std::io::stdout();

    if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        // Piped input: run each line as a turn, then exit. Keeps the REPL scriptable.
        let lines: Vec<String> = std::io::stdin().lines().collect::<Result<_, _>>()?;
        return run_loop(&mut transport, lines.into_iter(), &mut stdout).await;
    }

    // One session for the whole interactive lifetime — `run_loop`'s per-call `CreateSession`
    // would mint a fresh session per input line, which is right for the piped path (one shot)
    // but wrong here.
    let session = create_session(&mut transport).await?;

    let mut rl = rustyline::DefaultEditor::new()?;
    loop {
        match rl.readline("otto> ") {
            Ok(line) => {
                let _ = rl.add_history_entry(line.as_str());
                let line = line.trim().to_string();
                if line.is_empty() {
                    continue;
                }
                tokio::select! {
                    result = run_one(&mut transport, session, line, &mut stdout) => {
                        result?;
                    }
                    _ = tokio::signal::ctrl_c() => {
                        // Ctrl-C mid-turn: abort and return to the prompt. The abort is
                        // best-effort — the turn is silenced, not cancelled (see
                        // `EmbeddedTransport::send`'s `Abort` arm), so it keeps running to
                        // completion holding the engine's turn lock; that is a known limitation
                        // of this slice, not something to work around here.
                        let _ = transport.send(Command::Abort { session }).await;
                    }
                }
            }
            // Ctrl-C at the prompt, or Ctrl-D: exit cleanly.
            Err(rustyline::error::ReadlineError::Interrupted)
            | Err(rustyline::error::ReadlineError::Eof) => return Ok(()),
            Err(e) => return Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use otto_cli::FakeTransport;
    use otto_protocol::{Command, Event, EventKind, ServerMessage, SessionId};

    use super::run_loop;

    #[tokio::test]
    async fn loop_sends_each_input_line_as_a_prompt() {
        let session = SessionId::new();
        let mut t = FakeTransport::new(vec![
            ServerMessage::Ready {
                session,
                capabilities: Default::default(),
            },
            ServerMessage::Event {
                event: Event {
                    seq: 0,
                    session,
                    kind: EventKind::TurnComplete { ok: true },
                },
            },
            ServerMessage::Event {
                event: Event {
                    seq: 1,
                    session,
                    kind: EventKind::TurnComplete { ok: true },
                },
            },
        ]);
        let mut out = Vec::new();

        run_loop(
            &mut t,
            vec!["first".to_string(), "second".to_string()].into_iter(),
            &mut out,
        )
        .await
        .unwrap();

        let prompts: Vec<String> = t
            .sent()
            .iter()
            .filter_map(|c| match c {
                Command::SendPrompt { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(prompts, vec!["first".to_string(), "second".to_string()]);
        assert!(t.sent().iter().any(|c| matches!(c, Command::CreateSession)));
    }

    #[tokio::test]
    async fn loop_renders_events_not_debug_output() {
        let session = SessionId::new();
        let mut t = FakeTransport::new(vec![
            ServerMessage::Ready {
                session,
                capabilities: Default::default(),
            },
            ServerMessage::Event {
                event: Event {
                    seq: 0,
                    session,
                    kind: EventKind::FileEdit {
                        path: PathBuf::from("a.rs"),
                        bytes_written: 7,
                    },
                },
            },
            ServerMessage::Event {
                event: Event {
                    seq: 1,
                    session,
                    kind: EventKind::TurnComplete { ok: true },
                },
            },
        ]);
        let mut out = Vec::new();
        run_loop(&mut t, vec!["go".to_string()].into_iter(), &mut out)
            .await
            .unwrap();

        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("edited a.rs"));
        assert!(
            !text.contains("EventKind"),
            "must never fall back to Debug rendering"
        );
    }

    #[tokio::test]
    async fn loop_reports_a_server_error_and_keeps_going() {
        let session = SessionId::new();
        let mut t = FakeTransport::new(vec![
            ServerMessage::Ready {
                session,
                capabilities: Default::default(),
            },
            ServerMessage::Error {
                message: "boom".to_string(),
            },
            ServerMessage::Event {
                event: Event {
                    seq: 0,
                    session,
                    kind: EventKind::TurnComplete { ok: true },
                },
            },
        ]);
        let mut out = Vec::new();
        // A failing turn must not end the session.
        run_loop(
            &mut t,
            vec!["a".to_string(), "b".to_string()].into_iter(),
            &mut out,
        )
        .await
        .unwrap();
        assert!(String::from_utf8(out).unwrap().contains("boom"));
    }

    /// Extra beyond the brief's Step 1: proves the `CreateSession`/`SendPrompt` split (R7) — one
    /// `CreateSession` no matter how many lines are driven through `run_loop`, and the same
    /// session id is reused for every `SendPrompt`.
    #[tokio::test]
    async fn loop_creates_exactly_one_session_for_all_lines() {
        let session = SessionId::new();
        let mut t = FakeTransport::new(vec![
            ServerMessage::Ready {
                session,
                capabilities: Default::default(),
            },
            ServerMessage::Event {
                event: Event {
                    seq: 0,
                    session,
                    kind: EventKind::TurnComplete { ok: true },
                },
            },
            ServerMessage::Event {
                event: Event {
                    seq: 1,
                    session,
                    kind: EventKind::TurnComplete { ok: true },
                },
            },
            ServerMessage::Event {
                event: Event {
                    seq: 2,
                    session,
                    kind: EventKind::TurnComplete { ok: true },
                },
            },
        ]);
        let mut out = Vec::new();

        run_loop(
            &mut t,
            vec!["a".to_string(), "b".to_string(), "c".to_string()].into_iter(),
            &mut out,
        )
        .await
        .unwrap();

        let creates = t
            .sent()
            .iter()
            .filter(|c| matches!(c, Command::CreateSession))
            .count();
        assert_eq!(creates, 1, "one CreateSession no matter how many lines");

        let sessions: Vec<SessionId> = t
            .sent()
            .iter()
            .filter_map(|c| match c {
                Command::SendPrompt { session, .. } => Some(*session),
                _ => None,
            })
            .collect();
        assert!(sessions.iter().all(|s| *s == session));
    }

    /// Blank input lines are skipped rather than sent as an empty prompt.
    #[tokio::test]
    async fn loop_skips_blank_lines() {
        let session = SessionId::new();
        let mut t = FakeTransport::new(vec![
            ServerMessage::Ready {
                session,
                capabilities: Default::default(),
            },
            ServerMessage::Event {
                event: Event {
                    seq: 0,
                    session,
                    kind: EventKind::TurnComplete { ok: true },
                },
            },
        ]);
        let mut out = Vec::new();

        run_loop(
            &mut t,
            vec!["   ".to_string(), "go".to_string()].into_iter(),
            &mut out,
        )
        .await
        .unwrap();

        let prompts: Vec<String> = t
            .sent()
            .iter()
            .filter_map(|c| match c {
                Command::SendPrompt { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(prompts, vec!["go".to_string()]);
    }

    /// A `recv()` that yields `None` mid-turn (engine disconnected) must not panic — `run_loop`
    /// returns `Ok(())` rather than hanging or erroring.
    #[tokio::test]
    async fn loop_ends_gracefully_when_the_transport_closes_mid_turn() {
        let session = SessionId::new();
        let mut t = FakeTransport::new(vec![ServerMessage::Ready {
            session,
            capabilities: Default::default(),
        }]);
        let mut out = Vec::new();

        run_loop(&mut t, vec!["go".to_string()].into_iter(), &mut out)
            .await
            .unwrap();
    }

    /// `Ready` never arriving (only unrelated frames) must not hang — `create_session` bails
    /// once the script is exhausted.
    #[tokio::test]
    async fn loop_errors_if_the_session_never_becomes_ready() {
        let session = SessionId::new();
        let mut t = FakeTransport::new(vec![ServerMessage::Event {
            event: Event {
                seq: 0,
                session,
                kind: EventKind::TurnComplete { ok: true },
            },
        }]);
        let mut out = Vec::new();

        let err = run_loop(&mut t, vec!["go".to_string()].into_iter(), &mut out)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("engine closed"));
    }
}
