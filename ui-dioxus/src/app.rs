use std::rc::Rc;

use dioxus::prelude::*;
use futures_util::StreamExt;
use otto_protocol::{CapabilitiesManifest, Command, EventKind, ServerMessage, SessionId};
use uuid::Uuid;

use crate::components::{
    ApprovalPanel, ConnectionForm, EventLog, PendingApproval, PromptBar, StatusLine,
};
use crate::net::url::{advance_last_seq, build_ws_url, should_apply};
use crate::net::view_model::{client_error_row, describe_event, error_row, ConnState, LogRow};
use crate::transport::{connect, Sink, SocketEvent};

#[component]
pub fn App() -> Element {
    let url = use_signal(|| "ws://127.0.0.1:8787".to_string());
    let token = use_signal(String::new);
    let mut conn = use_signal(|| ConnState::Disconnected);
    let mut rows = use_signal(Vec::<LogRow>::new);
    let mut last_seq = use_signal(|| None::<u64>);
    let mut session = use_signal(|| None::<String>);
    // The capabilities manifest from the last `Ready` frame; None when disconnected (cleared on
    // every disconnect path so the strip never shows a stale manifest from a prior connection).
    let mut capabilities = use_signal(|| None::<CapabilitiesManifest>);
    // The live outbound sink; None when disconnected.
    let mut sink = use_signal(|| None::<Rc<dyn Sink>>);
    // The pending diff awaiting Approve/Reject, if any; None when idle or disconnected (cleared
    // on TurnComplete, on a server Error, on the decision itself, and on every disconnect path).
    let mut pending_approval = use_signal(|| None::<PendingApproval>);
    // Running token/cost meter for the current turn; None until the first TokenCostMeter event
    // (reset at the start of every new turn and on every disconnect path, matching `ui/`).
    let mut meter = use_signal(|| None::<(u64, u64)>);
    // Whether the current turn is paused (set by pause/resume; reset on a new turn and on every
    // disconnect path, matching `ui/`).
    let mut paused = use_signal(|| false);
    // Monotonic connection id. Bumped on every connect/disconnect; each per-connection drain task
    // captures the value current when it was spawned and bails the instant `generation` moves on,
    // so a superseded socket's already-queued event can never write the new connection's state.
    let mut generation = use_signal(|| 0u64);

    let mut do_connect = move || {
        let base = url.read().clone();
        let tok = token.read().clone();
        if base.trim().is_empty() || tok.trim().is_empty() {
            rows.write()
                .push(client_error_row("URL and token are required"));
            return;
        }
        let target = build_ws_url(&base, &tok, session.read().as_deref(), *last_seq.read());

        // Invalidate any live drain task and tear down the previous socket BEFORE installing the
        // new one: bump the generation (so the old task bails), then `close()` the old sink (so the
        // real socket stops delivering — dropping the `Rc` alone would not, on web).
        let my_gen = generation() + 1;
        generation.set(my_gen);
        if let Some(old) = sink.write().take() {
            old.close();
        }
        capabilities.set(None);
        pending_approval.set(None);
        meter.set(None);
        paused.set(false);

        conn.set(ConnState::Connecting);
        match connect(&target) {
            Ok((s, mut rx)) => {
                sink.set(Some(Rc::from(s)));
                // A fresh task per connection owns this receiver. Every shared-state write is
                // guarded by the generation check, so once a newer connect/disconnect bumps
                // `generation` this task stops touching state and exits.
                spawn(async move {
                    while let Some(ev) = rx.next().await {
                        // Stale-connection guard: a superseded task must not write the new
                        // connection's `conn`/`rows`/`sink`/`session`/`last_seq`.
                        if generation() != my_gen {
                            break;
                        }
                        match ev {
                            SocketEvent::Message(Ok(ServerMessage::Ready {
                                session: s,
                                capabilities: caps,
                                ..
                            })) => {
                                let id = s.0.to_string();
                                session.set(Some(id.clone()));
                                capabilities.set(Some(caps));
                                conn.set(ConnState::Connected { session: id });
                            }
                            SocketEvent::Message(Ok(ServerMessage::Event { event })) => {
                                let current = *last_seq.read();
                                if should_apply(current, event.seq) {
                                    last_seq.set(advance_last_seq(current, event.seq));
                                    if let EventKind::ApprovalRequest { id, path, old, new } =
                                        &event.kind
                                    {
                                        pending_approval.set(Some((
                                            *id,
                                            path.clone(),
                                            old.clone(),
                                            new.clone(),
                                        )));
                                    }
                                    if let EventKind::TokenCostMeter {
                                        input_tokens,
                                        output_tokens,
                                    } = &event.kind
                                    {
                                        meter.set(Some((*input_tokens, *output_tokens)));
                                    }
                                    // The turn ending resolves any outstanding approval (the
                                    // orchestrator parks on the approval *before* emitting
                                    // TurnComplete, so this never clears a genuinely pending
                                    // one). On reconnect this also clears a replayed-but-stale
                                    // request whose turn already finished fail-closed.
                                    if let EventKind::TurnComplete { .. } = &event.kind {
                                        pending_approval.set(None);
                                        paused.set(false);
                                    }
                                    rows.write().push(describe_event(&event.kind));
                                }
                            }
                            SocketEvent::Message(Ok(ServerMessage::Error { message })) => {
                                rows.write().push(error_row(&message));
                                // An Error frame is turn-terminal (the orchestrator emits
                                // TurnComplete only on success), so clear turn-scoped state here
                                // too — otherwise Pause/Resume would stay stuck on "Resume" until
                                // the next turn or reconnect.
                                pending_approval.set(None);
                                paused.set(false);
                            }
                            SocketEvent::Message(Ok(_)) => {}
                            SocketEvent::Message(Err(detail)) => {
                                rows.write().push(client_error_row(&detail));
                            }
                            SocketEvent::Closed | SocketEvent::Errored => {
                                // Guarded above, so this is the CURRENT connection's close — safe
                                // to flip to Disconnected and null the sink/capabilities.
                                conn.set(ConnState::Disconnected);
                                sink.set(None);
                                capabilities.set(None);
                                pending_approval.set(None);
                                meter.set(None);
                                paused.set(false);
                                break;
                            }
                        }
                    }
                });
            }
            Err(e) => {
                rows.write().push(client_error_row(&e));
                conn.set(ConnState::Disconnected);
            }
        }
    };

    let mut disconnect = move || {
        // Invalidate the live drain task, then actually close the socket (not just drop the `Rc`).
        generation.set(generation() + 1);
        if let Some(s) = sink.write().take() {
            s.close();
        }
        conn.set(ConnState::Disconnected);
        capabilities.set(None);
        pending_approval.set(None);
        meter.set(None);
        paused.set(false);
    };

    let mut send = move |cmd: Command| {
        if let Some(s) = sink.read().as_ref() {
            if let Err(e) = s.send(&cmd) {
                rows.write().push(client_error_row(&e));
            }
        }
    };

    let mut send_prompt = move |text: String| {
        if let Some(sid) = session.read().clone() {
            if let Ok(uuid) = Uuid::parse_str(&sid) {
                meter.set(None); // a new turn starts fresh
                paused.set(false);
                send(Command::SendPrompt {
                    session: SessionId(uuid),
                    text,
                });
            }
        }
    };
    let abort = move |_| {
        if let Some(sid) = session.read().clone() {
            if let Ok(uuid) = Uuid::parse_str(&sid) {
                send(Command::Abort {
                    session: SessionId(uuid),
                });
                paused.set(false); // the aborted turn is gone; don't leave the button on "Resume"
            }
        }
    };
    let mut pause = move |_| {
        if let Some(sid) = session.read().clone() {
            if let Ok(uuid) = Uuid::parse_str(&sid) {
                send(Command::Pause {
                    session: SessionId(uuid),
                });
                paused.set(true);
            }
        }
    };
    let mut resume = move |_| {
        if let Some(sid) = session.read().clone() {
            if let Ok(uuid) = Uuid::parse_str(&sid) {
                send(Command::Resume {
                    session: SessionId(uuid),
                });
                paused.set(false);
            }
        }
    };
    let mut decide = move |(id, approved): (Uuid, bool)| {
        let Some(sid) = session.read().clone() else {
            return;
        };
        let Ok(uuid) = Uuid::parse_str(&sid) else {
            return;
        };
        let cmd = Command::ApproveDiff {
            session: SessionId(uuid),
            id,
            approved,
        };
        // Only dismiss the panel once the verdict is actually on the wire. If the send fails the
        // orchestrator is still blocked on this approval, so keep the panel up for a retry rather
        // than silently dropping the diff.
        let Some(s) = sink.read().clone() else {
            return;
        };
        match s.send(&cmd) {
            Ok(()) => pending_approval.set(None),
            Err(e) => rows.write().push(client_error_row(&e)),
        }
    };

    rsx! {
        div { class: "app",
            StatusLine { conn, last_seq, capabilities, meter }
            EventLog { rows }
            ApprovalPanel {
                pending: pending_approval,
                on_decide: move |d| decide(d),
            }
            PromptBar {
                conn,
                paused,
                on_send: move |t| send_prompt(t),
                on_abort: abort,
                on_pause: move |p| pause(p),
                on_resume: move |r| resume(r),
            }
            ConnectionForm {
                url, token, conn,
                on_connect: move |_| do_connect(),
                on_disconnect: move |_| disconnect(),
            }
        }
    }
}
