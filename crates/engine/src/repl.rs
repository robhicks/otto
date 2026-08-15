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

use std::future::Future;
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
            //
            // Known, narrower residual risk, left open by this fix round: if
            // `record_turn`/`set_terminal_status` fails *after* a turn's own `TurnComplete` has
            // already streamed (`EngineService::run_prompt_with_controls` drains the orchestrator's
            // events — including `TurnComplete` — into the sink *before* persisting the turn
            // record; a persist failure after that point still turns into a `ServerMessage::Error`
            // on the wire), the resulting stray `Error` is queued and gets read by whichever
            // `recv()` happens next — the same failure mode `run_one_interruptible`'s drain fixes
            // for `Abort`. It cannot be fixed the same way here: unlike after `Abort`, an ordinary
            // `Error` is not always followed by a `TurnComplete` (a mid-turn failure never produces
            // one at all — see `loop_reports_a_server_error_and_keeps_going`), so draining
            // unconditionally after this arm would sometimes swallow a *different*, later turn's
            // real terminal frame instead of fixing anything. Closing this needs either reordering
            // `run_prompt_with_controls` so persistence can fail before the client ever sees
            // `TurnComplete`, or a non-blocking peek on `ClientTransport` — both out of scope for
            // this round (no `recv()` timeout/peek is being added here either).
            Some(ServerMessage::Error { message }) => {
                writeln!(out, "error: {message}")?;
                return Ok(());
            }
            Some(_) => continue,
            None => return Ok(()),
        }
    }
}

/// `run_one`, interruptible by `interrupt`: if `interrupt` resolves before the turn completes,
/// send `Command::Abort` and drain the transport up to (and including) the terminal frame the
/// abort is guaranteed to produce, before returning.
///
/// The drain is why this exists separately from an inline `tokio::select!` in `repl()`: when
/// `interrupt` wins the race, `run_one`'s future is dropped mid-`recv()`, and whatever the
/// still-running turn had already queued — plus the `Abort`-synthesized terminal `TurnComplete`
/// (`EmbeddedTransport::send`'s `Abort` arm always produces one, turn or no turn) — is left
/// sitting unread. `ClientTransport::recv()` never yields `None` on its own for a live transport,
/// so without draining here, the *next* `run_one` call's first `recv()` would silently consume one
/// of these stale frames instead of its own turn's first frame, permanently shifting every
/// subsequent turn's output by one prompt. Generic and transport-agnostic like `run_one`, so it is
/// testable against `FakeTransport` with no terminal and no real signal handler.
async fn run_one_interruptible<T: ClientTransport>(
    transport: &mut T,
    session: SessionId,
    text: String,
    out: &mut impl Write,
    interrupt: impl Future<Output = ()>,
) -> anyhow::Result<()> {
    tokio::select! {
        result = run_one(transport, session, text, out) => result,
        _ = interrupt => {
            let _ = transport.send(Command::Abort { session }).await;
            drain_to_terminal(transport).await;
            Ok(())
        }
    }
}

/// Discard frames until (and including) a `TurnComplete` event, or the transport closes.
///
/// Only safe to call where a terminal frame is *guaranteed* to eventually arrive — exactly the
/// case right after `Command::Abort` (see `run_one_interruptible`'s doc). Draining unconditionally
/// after an ordinary `ServerMessage::Error` would be wrong: some real failures never produce a
/// following `TurnComplete` at all, and draining there would swallow a *later* turn's own terminal
/// frame instead — see the residual-risk note on `run_one`'s `Error` arm.
async fn drain_to_terminal<T: ClientTransport>(transport: &mut T) {
    while let Some(msg) = transport.recv().await {
        if matches!(
            &msg,
            ServerMessage::Event { event } if matches!(event.kind, EventKind::TurnComplete { .. })
        ) {
            break;
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

    // The piped path above runs `run_loop` once, which creates and owns its own session for that
    // one call. The interactive path instead needs a single session to outlive many `run_one`
    // calls — one per readline iteration — so it is created here rather than by delegating to
    // `run_loop`.
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
                // Ctrl-C mid-turn: abort and return to the prompt. The abort is best-effort — the
                // turn is silenced, not cancelled (see `EmbeddedTransport::send`'s `Abort` arm),
                // so it keeps running to completion holding the engine's turn lock; that is a
                // known limitation of this slice, not something to work around here.
                // `run_one_interruptible` drains to the abort's terminal frame before returning,
                // so the *next* prompt does not inherit a stale frame from this one.
                run_one_interruptible(&mut transport, session, line, &mut stdout, async {
                    let _ = tokio::signal::ctrl_c().await;
                })
                .await?;
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

    use otto_cli::{ClientTransport, FakeTransport};
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

    /// A transport that yields to the executor at least once per `recv`, wrapping a
    /// `FakeTransport`.
    ///
    /// `FakeTransport::recv` resolves synchronously (a plain `VecDeque::pop_front`, no real
    /// suspension), so racing it inside `tokio::select!` against an already-ready `interrupt`
    /// future is not a fair race at all: `run_one`'s whole call chain would run to completion on
    /// `select!`'s very first poll, before `interrupt` is ever consulted, so the interrupt branch
    /// could never win. A real transport's `recv()` always has a genuine await point (it is
    /// reading off a channel/socket), so this wrapper is what makes the interrupt race realistic
    /// and, by forcing `run_one` to yield at its very first `recv`, deterministic: `interrupt`
    /// (immediately ready) is guaranteed to be the only branch ready on `select!`'s first poll.
    struct YieldingTransport {
        inner: FakeTransport,
    }

    #[async_trait::async_trait]
    impl ClientTransport for YieldingTransport {
        async fn send(&mut self, cmd: Command) -> anyhow::Result<()> {
            self.inner.send(cmd).await
        }

        async fn recv(&mut self) -> Option<ServerMessage> {
            tokio::task::yield_now().await;
            self.inner.recv().await
        }
    }

    /// The regression test for the Critical fix: after an interrupt, the *next* `SendPrompt` must
    /// start from a clean queue rather than immediately consuming a frame left over from the
    /// interrupted turn.
    ///
    /// Without `run_one_interruptible`'s drain, this test fails: the interrupted call's dropped
    /// `run_one` future never reads the stray `AgentStarted` event or the `Abort`-synthesized
    /// `TurnComplete { ok: false }`, so the *next* `run_one` call reads that stale
    /// `TurnComplete { ok: false }` as if it were its own first frame and reports "turn failed"
    /// instead of ever seeing its real `TurnComplete { ok: true }`.
    #[tokio::test]
    async fn interrupt_drains_the_aborted_turns_frames_before_the_next_prompt() {
        let session = SessionId::new();
        let mut t = YieldingTransport {
            inner: FakeTransport::new(vec![
                ServerMessage::Ready {
                    session,
                    capabilities: Default::default(),
                },
                // The interrupted turn: one real event already in flight, then the terminal
                // frame `EmbeddedTransport::send`'s `Abort` arm always synthesizes.
                ServerMessage::Event {
                    event: Event {
                        seq: 0,
                        session,
                        kind: EventKind::AgentStarted {
                            role: otto_protocol::Role::Planner,
                        },
                    },
                },
                ServerMessage::Event {
                    event: Event {
                        seq: 1,
                        session,
                        kind: EventKind::TurnComplete { ok: false },
                    },
                },
                // The *next* prompt's own, real frame.
                ServerMessage::Event {
                    event: Event {
                        seq: 2,
                        session,
                        kind: EventKind::TurnComplete { ok: true },
                    },
                },
            ]),
        };

        let session = super::create_session(&mut t).await.unwrap();
        let mut interrupted_out = Vec::new();

        // `interrupt` is already resolved, so it is guaranteed to win the race against
        // `YieldingTransport`'s forced-yield `run_one` (see `YieldingTransport`'s doc).
        super::run_one_interruptible(
            &mut t,
            session,
            "interrupted".to_string(),
            &mut interrupted_out,
            std::future::ready(()),
        )
        .await
        .unwrap();

        let mut next_out = Vec::new();
        super::run_one(&mut t, session, "next".to_string(), &mut next_out)
            .await
            .unwrap();
        let next_text = String::from_utf8(next_out).unwrap();
        assert!(
            next_text.contains("done"),
            "the next prompt must see its own TurnComplete, not a stale one left over from the \
             interrupted turn: {next_text}"
        );
    }

    /// The non-interrupted path of `run_one_interruptible` behaves exactly like `run_one`: when
    /// `interrupt` never resolves, the turn's own frames are rendered and no `Abort` is sent.
    #[tokio::test]
    async fn run_one_interruptible_delegates_to_run_one_when_not_interrupted() {
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
        let session = super::create_session(&mut t).await.unwrap();
        let mut out = Vec::new();

        // `std::future::pending` never resolves, so this exercises only the `run_one` branch.
        super::run_one_interruptible(
            &mut t,
            session,
            "go".to_string(),
            &mut out,
            std::future::pending(),
        )
        .await
        .unwrap();

        assert!(String::from_utf8(out).unwrap().contains("done"));
        assert!(
            !t.sent().iter().any(|c| matches!(c, Command::Abort { .. })),
            "no interrupt fired, so no Abort should have been sent"
        );
    }
}
