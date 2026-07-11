use std::rc::Rc;

use dioxus::prelude::*;
use futures_channel::mpsc::UnboundedReceiver;
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
    // The inbound receiver for the current socket, handed to the drain future on connect.
    let mut incoming = use_signal(|| None::<UnboundedReceiver<SocketEvent>>);

    // Drain loop: whenever a new receiver is installed, pull events until the socket closes.
    use_future(move || async move {
        loop {
            // Take the receiver out (if any) and drain it fully, then wait for the next connect.
            let rx = incoming.write().take();
            if let Some(mut rx) = rx {
                while let Some(ev) = rx.next().await {
                    match ev {
                        SocketEvent::Message(Ok(ServerMessage::Ready { session: s, .. })) => {
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
                            conn.set(ConnState::Disconnected);
                            sink.set(None);
                            break;
                        }
                    }
                }
            }
            // Yield so we don't busy-spin when idle. We only re-enter the `if let Some` arm once
            // `incoming` is repopulated by the next `connect()`, so a short cooperative yield
            // between polls is enough — no need for an explicit dependency/wakeup channel.
            gloo_or_tokio_yield().await;
        }
    });

    let mut do_connect = move || {
        let base = url.read().clone();
        let tok = token.read().clone();
        if base.trim().is_empty() || tok.trim().is_empty() {
            rows.write()
                .push(client_error_row("URL and token are required"));
            return;
        }
        let target = build_ws_url(&base, &tok, session.read().as_deref(), *last_seq.read());
        conn.set(ConnState::Connecting);
        match connect(&target) {
            Ok((s, rx)) => {
                sink.set(Some(Rc::from(s)));
                incoming.set(Some(rx));
            }
            Err(e) => {
                rows.write().push(client_error_row(&e));
                conn.set(ConnState::Disconnected);
            }
        }
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
                on_disconnect: move |_| {
                    sink.set(None);
                    conn.set(ConnState::Disconnected);
                },
            }
        }
    }
}

/// Cross-target cooperative yield: gives the executor a chance to run other tasks (e.g. deliver
/// the `unbounded_send` that `connect()` just triggered) between drain-loop poll attempts,
/// without busy-spinning while `incoming` is empty.
async fn gloo_or_tokio_yield() {
    #[cfg(feature = "web")]
    gloo_timers::future::TimeoutFuture::new(0).await;
    #[cfg(feature = "desktop")]
    tokio::task::yield_now().await;
    #[cfg(not(any(feature = "web", feature = "desktop")))]
    std::future::pending::<()>().await;
}
