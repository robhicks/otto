use leptos::prelude::*;
use otto_protocol::{Command, ServerMessage, SessionId};
use uuid::Uuid;
use web_sys::WebSocket;

use crate::components::{ConnectionForm, EventLog, PromptBar, StatusLine};
use crate::url::{advance_last_seq, build_ws_url, should_apply};
use crate::view_model::{client_error_row, describe_event, error_row, ConnState, LogRow};
use crate::ws::{open_ws, send_command};

#[component]
pub fn App() -> impl IntoView {
    // Form inputs.
    let url = RwSignal::new("ws://127.0.0.1:8787".to_string());
    let token = RwSignal::new(String::new());

    // Connection + stream state.
    let conn = RwSignal::new(ConnState::Disconnected);
    let rows = RwSignal::new(Vec::<LogRow>::new());
    let last_seq = RwSignal::new(None::<u64>); // retained across disconnects for replay
    let session = RwSignal::new(None::<String>); // retained across disconnects for reconnect
    let socket = RwSignal::new(None::<WebSocket>);

    // Connect (also used for reconnect: session/last_seq are appended when present).
    let connect = move || {
        let base = url.get();
        let tok = token.get();
        if base.trim().is_empty() || tok.trim().is_empty() {
            rows.update(|v| v.push(client_error_row("URL and token are required")));
            return;
        }
        let target = build_ws_url(&base, &tok, session.get().as_deref(), last_seq.get());
        // Close any existing socket before opening a new one so its callbacks can't
        // later flip the UI state out from under the new connection.
        if let Some(old) = socket.get_untracked() {
            let _ = old.close();
            socket.set(None);
        }
        conn.set(ConnState::Connecting);

        let on_msg = move |incoming: Result<ServerMessage, String>| match incoming {
            Ok(ServerMessage::Ready { session: s }) => {
                let id = s.0.to_string();
                session.set(Some(id.clone()));
                conn.set(ConnState::Connected { session: id });
            }
            Ok(ServerMessage::Event { event }) => {
                if should_apply(last_seq.get_untracked(), event.seq) {
                    last_seq.set(advance_last_seq(last_seq.get_untracked(), event.seq));
                    rows.update(|v| v.push(describe_event(&event.kind)));
                }
            }
            Ok(ServerMessage::Error { message }) => {
                rows.update(|v| v.push(error_row(&message)));
            }
            Err(detail) => {
                rows.update(|v| v.push(client_error_row(&detail)));
            }
        };
        let on_close = move || conn.set(ConnState::Disconnected);
        let on_error = move || {
            rows.update(|v| v.push(client_error_row("connection rejected — check URL/token")));
            conn.set(ConnState::Disconnected);
        };

        match open_ws(&target, on_msg, on_close, on_error) {
            Ok(ws) => socket.set(Some(ws)),
            Err(e) => {
                rows.update(|v| v.push(client_error_row(&e)));
                conn.set(ConnState::Disconnected);
            }
        }
    };

    let disconnect = move || {
        if let Some(ws) = socket.get() {
            let _ = ws.close();
        }
        socket.set(None);
        conn.set(ConnState::Disconnected);
    };

    // The server ignores the `session` field of an inbound Command, but the type requires it;
    // fill it from the retained session id.
    let send_prompt = move |text: String| {
        let (Some(ws), Some(sid)) = (socket.get(), session.get()) else {
            return;
        };
        let Ok(uuid) = Uuid::parse_str(&sid) else {
            return;
        };
        let cmd = Command::SendPrompt { session: SessionId(uuid), text };
        if let Err(e) = send_command(&ws, &cmd) {
            rows.update(|v| v.push(client_error_row(&e)));
        }
    };

    let abort = move || {
        let (Some(ws), Some(sid)) = (socket.get(), session.get()) else {
            return;
        };
        if let Ok(uuid) = Uuid::parse_str(&sid) {
            let _ = send_command(&ws, &Command::Abort { session: SessionId(uuid) });
        }
    };

    view! {
        <div class="app">
            <StatusLine conn=conn last_seq=last_seq />
            <EventLog rows=rows />
            <PromptBar
                conn=conn
                on_send=Callback::new(send_prompt)
                on_abort=Callback::new(move |_| abort())
            />
            <ConnectionForm
                url=url
                token=token
                conn=conn
                on_connect=Callback::new(move |_| connect())
                on_disconnect=Callback::new(move |_| disconnect())
            />
        </div>
    }
}
