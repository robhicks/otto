use std::path::PathBuf;
use std::rc::Rc;

use dioxus::prelude::*;
use futures_util::StreamExt;
use otto_protocol::{CapabilitiesManifest, Command, EventKind, ServerMessage, SessionId};
use uuid::Uuid;

use crate::components::{
    ApprovalPanel, ConnectionForm, EventLog, FileTree, PendingApproval, PromptBar, StatusLine,
};
use crate::editor::Editor;
use crate::net::tree::{build_tree, decode_or_binary, FileBody, TreeNode};
use crate::net::url::{advance_last_seq, build_ws_url, should_apply, ws_to_http_base};
use crate::net::view_model::{
    can_demote, can_promote, client_error_row, describe_event, error_row, ConnState, LogRow,
};
use crate::transport::{connect, list_files, read_file, Sink, SocketEvent};

// Cross-target stylesheet: `document::Stylesheet` + the `asset!()` manganis macro load
// `style.css` on BOTH the `web` (wasm) and `desktop` (native webview) targets, so neither
// build depends on the web-only `Dioxus.toml` `[web.resource]` path alone (which the desktop
// target never reads). The path is relative to this crate's `Cargo.toml`.
static STYLE_CSS: Asset = asset!("/style.css");

#[component]
pub fn App() -> Element {
    let mut url = use_signal(|| "ws://127.0.0.1:8787".to_string());
    // `mut`: the desktop-only auto-connect mount block below calls `token.set(..)` with the
    // sidecar-generated bearer token (`Signal::set` requires `&mut self`, so this binding must
    // be mutable even though the web target never writes it — only reads it via `token.read()`).
    let mut token = use_signal(String::new);
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
    // Whether a turn is currently running: true on AgentStarted (and immediately on
    // send_prompt), false on TurnComplete/Error and every disconnect path (matching `ui/`).
    // Gates the Promote/Demote buttons — handing off mid-turn would snapshot partial state.
    let mut turn_running = use_signal(|| false);
    // Set by a Promoted/Demoted frame; an Effect below performs the actual reconnect to the
    // handed-back endpoint (the drain task can't call `do_connect` directly — it's defined
    // outside the task and borrowing it there would fight the `spawn`'s `'static` bound).
    let mut reconnect_to = use_signal(|| None::<String>);
    // Monotonic connection id. Bumped on every connect/disconnect; each per-connection drain task
    // captures the value current when it was spawned and bails the instant `generation` moves on,
    // so a superseded socket's already-queued event can never write the new connection's state.
    let mut generation = use_signal(|| 0u64);

    // Workspace tree + editor state (unhighlighted, controlled-buffer editor for this slice).
    let mut tree = use_signal(Vec::<TreeNode>::new);
    let mut open_file = use_signal(|| None::<(PathBuf, FileBody)>);
    let mut editor_seed = use_signal(String::new);

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
        turn_running.set(false);

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
                                    if let EventKind::AgentStarted { .. } = &event.kind {
                                        turn_running.set(true);
                                    }
                                    // The turn ending resolves any outstanding approval (the
                                    // orchestrator parks on the approval *before* emitting
                                    // TurnComplete, so this never clears a genuinely pending
                                    // one). On reconnect this also clears a replayed-but-stale
                                    // request whose turn already finished fail-closed.
                                    if let EventKind::TurnComplete { .. } = &event.kind {
                                        pending_approval.set(None);
                                        paused.set(false);
                                        turn_running.set(false);
                                    }
                                    rows.write().push(describe_event(&event.kind));
                                }
                            }
                            SocketEvent::Message(Ok(ServerMessage::Error { message })) => {
                                rows.write().push(error_row(&message));
                                // An Error frame is turn-terminal (the orchestrator emits
                                // TurnComplete only on success), so clear turn-scoped state here
                                // too — otherwise Pause/Resume/Promote/Demote would stay stuck
                                // until the next turn or reconnect.
                                pending_approval.set(None);
                                paused.set(false);
                                turn_running.set(false);
                            }
                            SocketEvent::Message(Ok(ServerMessage::Promoted {
                                endpoint, ..
                            }))
                            | SocketEvent::Message(Ok(ServerMessage::Demoted {
                                endpoint, ..
                            })) => {
                                // Reconnect to the handed-back engine, reusing token + session +
                                // last_seq. The new engine's manifest flips the status strip
                                // local<->remote. Deferred to an Effect (this task can't call
                                // `do_connect` itself — it's defined outside the `spawn`).
                                reconnect_to.set(Some(endpoint));
                            }
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
                                turn_running.set(false);
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
        turn_running.set(false);
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
                turn_running.set(true);
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
    let mut promote_remote = move |_| {
        if let Some(sid) = session.read().clone() {
            if let Ok(uuid) = Uuid::parse_str(&sid) {
                send(Command::PromoteToRemote {
                    session: SessionId(uuid),
                });
            }
        }
    };
    let mut demote_local = move |_| {
        if let Some(sid) = session.read().clone() {
            if let Ok(uuid) = Uuid::parse_str(&sid) {
                send(Command::DemoteToLocal {
                    session: SessionId(uuid),
                });
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

    // Fetch the file list over the /workspace RPC and rebuild the tree. No-op without url+token
    // (the form gates Connect on both, so by Connected they are present). Spawned the same way
    // the drain task is — an ordinary Dioxus `spawn`, not a `use_future` (this is an action
    // triggered by an event/effect, not a standing background task).
    let load_files = move || {
        let base = url.read().clone();
        let http_base = ws_to_http_base(&base);
        let tok = token.read().clone();
        if http_base.is_empty() || tok.is_empty() {
            return;
        }
        spawn(async move {
            match list_files(&http_base, &tok).await {
                Ok(paths) => tree.set(build_tree(&paths)),
                Err(e) => rows.write().push(client_error_row(&e)),
            }
        });
    };

    // Read a file and mount it in the editor (or show a binary/oversize notice). No-op unless
    // Connected, matching `ui/src/app.rs`'s `open_path` guard.
    let open_path = move |path: PathBuf| {
        if !matches!(conn(), ConnState::Connected { .. }) {
            return;
        }
        let base = url.read().clone();
        let http_base = ws_to_http_base(&base);
        let tok = token.read().clone();
        if http_base.is_empty() || tok.is_empty() {
            return;
        }
        spawn(async move {
            match read_file(&http_base, &tok, path.clone()).await {
                Ok(bytes) => {
                    let body = decode_or_binary(&bytes);
                    // Only text files seed the editor; for Binary/TooLarge, Editor shows a
                    // notice instead of mounting the buffer, so a stale `editor_seed` is never
                    // read (matches `ui/src/app.rs`'s open_path comment).
                    if let FileBody::Text(ref s) = body {
                        editor_seed.set(s.clone());
                    }
                    open_file.set(Some((path, body)));
                }
                Err(e) => rows.write().push(client_error_row(&e)),
            }
        });
    };

    // Auto-load the tree when the connection reaches Connected. `conn()` is a TRACKED READ —
    // that is what subscribes this effect to the signal, so the drain task's later
    // `conn.set(ConnState::Connected { .. })` actually re-fires it. A write-guard access
    // (`conn.write()`) would not subscribe and this would never fire — the same class of bug
    // the handover-reconnect effect below already documents. Mirrors `ui/src/app.rs`'s
    // "Auto-load the tree when the connection reaches Connected" Effect.
    use_effect(move || {
        if matches!(conn(), ConnState::Connected { .. }) {
            load_files();
        }
    });

    // Perform a handover reconnect: point the URL at the new endpoint and reconnect through the
    // same hardened `do_connect` used by the manual Connect button — it bumps `generation`,
    // closes the old sink, opens the new socket, and spawns a fresh generation-guarded drain
    // task, reusing session + last_seq (via `build_ws_url`) for replay.
    //
    // `reconnect_to()` is a *tracked read* — that is what subscribes this effect to the signal,
    // so a later `reconnect_to.set(Some(endpoint))` from the drain task actually re-fires it.
    // (A `.write().take()` would be a write-guard access, which never subscribes — the effect
    // would capture zero deps on its first `None` run and never wake again.) Order matters and is
    // deliberate: read (subscribe) → `set(None)` (clear) → connect. The `set(None)` re-runs the
    // effect once more; that run reads `None`, skips the body, and settles — bounded, not a loop.
    // Mirrors `ui/src/app.rs`'s handover `Effect` exactly.
    use_effect(move || {
        if let Some(endpoint) = reconnect_to() {
            reconnect_to.set(None);
            url.set(endpoint);
            do_connect();
        }
    });

    // Desktop-only: on launch, pick a workspace folder, spawn a local `otto serve` sidecar on a
    // fixed port with a freshly-generated token, and auto-connect — reproducing the Tauri
    // `desktop/` wrapper's UX (sub-project G) inside this one crate, no separate wrapper crate
    // and no `ui/dist` sidecar handoff. This whole block (and the `use_signal`/`use_future` hooks
    // it adds) is compiled in only under `--features desktop`; the web build has neither the
    // block nor the hooks, so there is no cross-target hook-order mismatch to worry about — each
    // target sees its own fixed, unconditional hook sequence every render. If `boot()` returns
    // `None` (the user cancelled the folder picker, or the sidecar failed to spawn), this is a
    // no-op and the manual `ConnectionForm` below stays the fallback.
    #[cfg(feature = "desktop")]
    {
        use crate::desktop_boot::BootOutcome;
        // Holds the live sidecar `Child` (spawned `kill_on_drop(true)`) so the process lives for the
        // app's lifetime and is killed when this signal's value is dropped. `None` until `boot()`
        // resolves to `Ready` (or forever, if the user cancels or the spawn fails).
        let mut sidecar = use_signal(|| None::<tokio::process::Child>);
        use_future(move || async move {
            // `boot()` already waits for the sidecar's readiness line before returning `Ready`, so
            // no fixed sleep is needed here — `do_connect()` fires as soon as the port is bound.
            match crate::desktop_boot::boot().await {
                BootOutcome::Ready(child, params) => {
                    sidecar.set(Some(child));
                    url.set(params.ws);
                    token.set(params.token);
                    do_connect();
                }
                // Spawn failure (missing/misconfigured `otto` binary): surface it so the user knows
                // why auto-connect didn't happen, then fall back to the manual form.
                BootOutcome::SpawnFailed(msg) => {
                    rows.write().push(client_error_row(&msg));
                }
                // User cancelled the picker: silent fallback to the manual ConnectionForm.
                BootOutcome::Cancelled => {}
            }
        });
    }

    rsx! {
        document::Stylesheet { href: STYLE_CSS }
        div { class: "app",
            StatusLine { conn, last_seq, capabilities, meter }
            div { class: "workspace",
                div { class: "workspace-side",
                    button {
                        class: "refresh-btn",
                        disabled: !matches!(conn(), ConnState::Connected { .. }),
                        onclick: move |_| load_files(),
                        "Refresh files"
                    }
                    FileTree {
                        nodes: tree.read().clone(),
                        on_open: move |p| open_path(p),
                    }
                }
                Editor { open: open_file, seed: editor_seed }
            }
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
            div { class: "handover",
                button {
                    disabled: !can_promote(&conn.read(), &capabilities.read(), *turn_running.read()),
                    onclick: move |_| promote_remote(()),
                    "Promote to remote"
                }
                button {
                    disabled: !can_demote(&conn.read(), &capabilities.read(), *turn_running.read()),
                    onclick: move |_| demote_local(()),
                    "Demote to local"
                }
            }
            ConnectionForm {
                url, token, conn,
                on_connect: move |_| do_connect(),
                on_disconnect: move |_| disconnect(),
            }
        }
    }
}
