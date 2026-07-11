use std::rc::Rc;

use dioxus::prelude::*;
use futures_util::StreamExt;
// Slice A references only these; later slices (D/E/F) add `EventKind` when they match on it.
use otto_protocol::{Command, ServerMessage, SessionId};
use uuid::Uuid;

use crate::components::{ConnectionForm, EventLog, PromptBar};
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
    // The live outbound sink; None when disconnected.
    let mut sink = use_signal(|| None::<Rc<dyn Sink>>);
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
                                session: s, ..
                            })) => {
                                let id = s.0.to_string();
                                session.set(Some(id.clone()));
                                conn.set(ConnState::Connected { session: id });
                            }
                            SocketEvent::Message(Ok(ServerMessage::Event { event })) => {
                                let current = *last_seq.read();
                                if should_apply(current, event.seq) {
                                    last_seq.set(advance_last_seq(current, event.seq));
                                    rows.write().push(describe_event(&event.kind));
                                }
                            }
                            SocketEvent::Message(Ok(ServerMessage::Error { message })) => {
                                rows.write().push(error_row(&message));
                            }
                            SocketEvent::Message(Ok(_)) => {}
                            SocketEvent::Message(Err(detail)) => {
                                rows.write().push(client_error_row(&detail));
                            }
                            SocketEvent::Closed | SocketEvent::Errored => {
                                // Guarded above, so this is the CURRENT connection's close — safe
                                // to flip to Disconnected and null the sink.
                                conn.set(ConnState::Disconnected);
                                sink.set(None);
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
            }
        }
    };

    rsx! {
        div { class: "app",
            EventLog { rows }
            PromptBar {
                conn,
                on_send: move |t| send_prompt(t),
                on_abort: abort,
            }
            ConnectionForm {
                url, token, conn,
                on_connect: move |_| do_connect(),
                on_disconnect: move |_| disconnect(),
            }
        }
    }
}
