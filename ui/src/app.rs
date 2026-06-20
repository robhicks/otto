use std::path::PathBuf;

use leptos::prelude::*;
use otto_protocol::{CapabilitiesManifest, Command, EventKind, ServerMessage, SessionId};
use uuid::Uuid;
use web_sys::WebSocket;

use crate::components::{
    ApprovalPanel, ConnectionForm, EditorPane, EventLog, FileTree, PromptBar, StatusLine,
};
use crate::tree::{build_tree, decode_or_binary, FileBody, TreeNode};
use crate::url::{advance_last_seq, build_ws_url, should_apply, ws_to_http_base};
use crate::view_model::{
    can_demote, can_promote, client_error_row, describe_event, error_row, ConnState, LogRow,
};
use crate::workspace::{list_files, read_file};
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
    // `capabilities`: set on Ready; cleared on every disconnect path (connect-start, explicit
    // disconnect, and the on_close/on_error socket drops). So `Some` ⟺ a live manifest for the
    // current connection; StatusLine additionally gates display on ConnState::Connected.
    let capabilities = RwSignal::new(None::<CapabilitiesManifest>);

    // Workspace tree + editor state.
    let tree = RwSignal::new(Vec::<TreeNode>::new());
    let open_file = RwSignal::new(None::<(PathBuf, FileBody)>);
    let editor_seed = RwSignal::new(String::new()); // file text set once at open time
    let editor_dirty = RwSignal::new(false);
    let pending_approval = RwSignal::new(None::<crate::components::PendingApproval>);
    let meter = RwSignal::new(None::<(u64, u64)>); // (input, output) tokens for the current turn
    let paused = RwSignal::new(false);
    let turn_running = RwSignal::new(false);
    // Set by a Promoted/Demoted frame; an Effect below performs the actual reconnect to the
    // handed-back endpoint (on_msg can't call `connect` directly — it's defined inside it).
    let reconnect_to = RwSignal::new(None::<String>);

    // Connect (also used for reconnect: session/last_seq are appended when present).
    let connect = move || {
        let base = url.get();
        let tok = token.get();
        if base.trim().is_empty() || tok.trim().is_empty() {
            rows.update(|v| v.push(client_error_row("URL and token are required")));
            return;
        }
        let target = build_ws_url(&base, &tok, session.get().as_deref(), last_seq.get());
        // Detach the old socket's handlers, then close it, so its (forget()-leaked)
        // callbacks can't fire late and flip the new connection's state.
        if let Some(old) = socket.get_untracked() {
            old.set_onmessage(None);
            old.set_onclose(None);
            old.set_onerror(None);
            let _ = old.close();
            socket.set(None);
        }
        conn.set(ConnState::Connecting);
        capabilities.set(None);
        pending_approval.set(None);
        meter.set(None);
        paused.set(false);
        turn_running.set(false);

        let on_msg = move |incoming: Result<ServerMessage, String>| match incoming {
            Ok(ServerMessage::Ready {
                session: s,
                capabilities: caps,
            }) => {
                let id = s.0.to_string();
                session.set(Some(id.clone()));
                capabilities.set(Some(caps));
                conn.set(ConnState::Connected { session: id });
            }
            Ok(ServerMessage::Event { event }) => {
                if should_apply(last_seq.get_untracked(), event.seq) {
                    last_seq.set(advance_last_seq(last_seq.get_untracked(), event.seq));
                    if let EventKind::ApprovalRequest { id, path, old, new } = &event.kind {
                        pending_approval.set(Some((*id, path.clone(), old.clone(), new.clone())));
                    }
                    if let EventKind::TokenCostMeter {
                        input_tokens,
                        output_tokens,
                    } = &event.kind
                    {
                        meter.set(Some((*input_tokens, *output_tokens)));
                    }
                    // The turn ending resolves any outstanding approval (the orchestrator parks on
                    // the approval *before* emitting TurnComplete, so this never clears a genuinely
                    // pending one). On reconnect this also clears a replayed-but-stale request whose
                    // turn already finished fail-closed.
                    if let EventKind::AgentStarted { .. } = &event.kind {
                        turn_running.set(true);
                    }
                    if let EventKind::TurnComplete { .. } = &event.kind {
                        pending_approval.set(None);
                        paused.set(false);
                        turn_running.set(false);
                    }
                    rows.update(|v| v.push(describe_event(&event.kind)));
                }
            }
            Ok(ServerMessage::Error { message }) => {
                rows.update(|v| v.push(error_row(&message)));
                // An Error frame is turn-terminal (the orchestrator emits TurnComplete only on
                // success), so clear turn-scoped state — otherwise Promote/Demote/Pause stay
                // disabled until reconnect. (No-op when the Error arrived between turns.)
                turn_running.set(false);
                paused.set(false);
                pending_approval.set(None);
            }
            Ok(ServerMessage::Promoted { endpoint, .. })
            | Ok(ServerMessage::Demoted { endpoint, .. }) => {
                // Reconnect to the handed-back engine, reusing token + session + last_seq. The new
                // engine's manifest flips the status strip local↔remote. Deferred to an Effect.
                reconnect_to.set(Some(endpoint));
            }
            Err(detail) => {
                rows.update(|v| v.push(client_error_row(&detail)));
            }
        };
        let on_close = move || {
            capabilities.set(None);
            pending_approval.set(None);
            meter.set(None);
            paused.set(false);
            turn_running.set(false);
            conn.set(ConnState::Disconnected);
        };
        let on_error = move || {
            rows.update(|v| v.push(client_error_row("connection rejected — check URL/token")));
            capabilities.set(None);
            pending_approval.set(None);
            meter.set(None);
            paused.set(false);
            turn_running.set(false);
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
        capabilities.set(None);
        pending_approval.set(None);
        meter.set(None);
        paused.set(false);
        turn_running.set(false);
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
        meter.set(None); // a new turn starts fresh
        paused.set(false);
        turn_running.set(true);
        let cmd = Command::SendPrompt {
            session: SessionId(uuid),
            text,
        };
        if let Err(e) = send_command(&ws, &cmd) {
            rows.update(|v| v.push(client_error_row(&e)));
        }
    };

    let abort = move || {
        let (Some(ws), Some(sid)) = (socket.get(), session.get()) else {
            return;
        };
        if let Ok(uuid) = Uuid::parse_str(&sid) {
            let _ = send_command(
                &ws,
                &Command::Abort {
                    session: SessionId(uuid),
                },
            );
            paused.set(false); // the aborted turn is gone; don't leave the button on "Resume"
        }
    };

    let pause = move || {
        let (Some(ws), Some(sid)) = (socket.get(), session.get()) else {
            return;
        };
        if let Ok(uuid) = Uuid::parse_str(&sid) {
            let _ = send_command(
                &ws,
                &Command::Pause {
                    session: SessionId(uuid),
                },
            );
            paused.set(true);
        }
    };
    let resume = move || {
        let (Some(ws), Some(sid)) = (socket.get(), session.get()) else {
            return;
        };
        if let Ok(uuid) = Uuid::parse_str(&sid) {
            let _ = send_command(
                &ws,
                &Command::Resume {
                    session: SessionId(uuid),
                },
            );
            paused.set(false);
        }
    };

    let promote_remote = move || {
        let (Some(ws), Some(sid)) = (socket.get(), session.get()) else {
            return;
        };
        if let Ok(uuid) = Uuid::parse_str(&sid) {
            let _ = send_command(
                &ws,
                &Command::PromoteToRemote {
                    session: SessionId(uuid),
                },
            );
        }
    };
    let demote_local = move || {
        let (Some(ws), Some(sid)) = (socket.get(), session.get()) else {
            return;
        };
        if let Ok(uuid) = Uuid::parse_str(&sid) {
            let _ = send_command(
                &ws,
                &Command::DemoteToLocal {
                    session: SessionId(uuid),
                },
            );
        }
    };

    let decide = move |(id, approved): (Uuid, bool)| {
        let (Some(ws), Some(sid)) = (socket.get(), session.get()) else {
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
        match send_command(&ws, &cmd) {
            Ok(()) => pending_approval.set(None),
            Err(e) => rows.update(|v| v.push(client_error_row(&e))),
        }
    };

    // Fetch the file list over the /workspace RPC and rebuild the tree. No-op without
    // url+token (the form gates Connect on both, so by Connected they are present).
    // A `Callback` so it can be shared between the Effect and the Refresh button.
    let load_files = Callback::new(move |_: ()| {
        let http_base = ws_to_http_base(&url.get());
        let tok = token.get();
        if http_base.is_empty() || tok.is_empty() {
            return;
        }
        leptos::task::spawn_local(async move {
            match list_files(&http_base, &tok).await {
                Ok(paths) => tree.set(build_tree(&paths)),
                Err(e) => rows.update(|v| v.push(client_error_row(&e))),
            }
        });
    });

    // Read a file and mount it in the editor (or show a binary/oversize notice).
    let open_path = move |path: PathBuf| {
        if !matches!(conn.get(), ConnState::Connected { .. }) {
            return;
        }
        let http_base = ws_to_http_base(&url.get());
        let tok = token.get();
        if http_base.is_empty() || tok.is_empty() {
            return;
        }
        leptos::task::spawn_local(async move {
            match read_file(&http_base, &tok, path.clone()).await {
                Ok(bytes) => {
                    let body = decode_or_binary(&bytes);
                    // editor_seed/editor_dirty are written ONLY here, in the file-open flow.
                    // Only text files seed the editor; for Binary/TooLarge, EditorPane shows a
                    // notice instead of mounting the editor, so a stale `editor_seed` is never read.
                    editor_dirty.set(false); // reset on every open; only text re-seeds the editor
                    if let FileBody::Text(ref s) = body {
                        editor_seed.set(s.clone());
                    }
                    open_file.set(Some((path, body)));
                }
                Err(e) => rows.update(|v| v.push(client_error_row(&e))),
            }
        });
    };

    // Auto-load the tree when the connection reaches Connected.
    Effect::new(move |_| {
        if matches!(conn.get(), ConnState::Connected { .. }) {
            load_files.run(());
        }
    });

    // Perform a handover reconnect: point the URL at the new endpoint and reconnect. `connect`
    // closes the old socket first and reuses session + last_seq for replay.
    Effect::new(move |_| {
        if let Some(endpoint) = reconnect_to.get() {
            reconnect_to.set(None);
            url.set(endpoint);
            connect();
        }
    });

    view! {
        <div class="app">
            <StatusLine conn=conn last_seq=last_seq capabilities=capabilities meter=meter />
            <div class="workspace">
                <div class="workspace-side">
                    <button
                        class="refresh-btn"
                        on:click=move |_| load_files.run(())
                        disabled=move || !matches!(conn.get(), ConnState::Connected { .. })
                    >
                        "Refresh files"
                    </button>
                    <FileTree
                        nodes=tree.into()
                        on_open=Callback::new(open_path)
                    />
                </div>
                <EditorPane
                    open=open_file.into()
                    seed=editor_seed.into()
                    dirty=editor_dirty
                />
            </div>
            <ApprovalPanel
                pending=pending_approval.into()
                on_decide=Callback::new(decide)
            />
            <EventLog rows=rows />
            <div class="handover">
                <button
                    on:click=move |_| promote_remote()
                    disabled=move || !can_promote(&conn.get(), &capabilities.get(), turn_running.get())
                >"Promote to remote"</button>
                <button
                    on:click=move |_| demote_local()
                    disabled=move || !can_demote(&conn.get(), &capabilities.get(), turn_running.get())
                >"Demote to local"</button>
            </div>
            <PromptBar
                conn=conn
                paused=paused
                on_send=Callback::new(send_prompt)
                on_abort=Callback::new(move |_| abort())
                on_pause=Callback::new(move |_| pause())
                on_resume=Callback::new(move |_| resume())
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
