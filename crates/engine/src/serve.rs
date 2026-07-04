//! WebSocket transport for the engine. Maps WS frames to `Command`/event frames over an
//! `EngineService`: bearer-token auth on upgrade, a `Ready { session }` frame on connect,
//! optional `Last-Event-ID` replay (`?last_seq=`), then live streamed events per `SendPrompt`.
//! Binds loopback (plaintext `ws://` or, with `serve::run` + a `RustlsConfig`, `wss://`); concurrent sessions are out of scope (see the design spec).

use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::Router as AxumRouter;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum_server::tls_rustls::RustlsConfig;
use futures_util::SinkExt;
use futures_util::stream::{SplitSink, StreamExt};
use otto_engine_core::tool::Approver;
use otto_engine_core::tool::PauseController;
use otto_protocol::{
    CapabilitiesManifest, Command, Event, ServerMessage, SessionId, WorkspaceRequest,
};
use serde::Deserialize;
use tokio::sync::Notify;
use tokio::sync::oneshot;
use tower_http::cors::{Any, CorsLayer};
use uuid::Uuid;

use crate::loopback::LoopbackTarget;
use crate::service::{EngineService, EventSink, TurnControls};
use otto_remote::{PromoteConfig, RemoteHandle, promote};

#[derive(Deserialize, Default)]
struct ConnectParams {
    session: Option<String>,
    last_seq: Option<u64>,
    /// Bearer token carried in the query string. A browser `WebSocket` can't set an
    /// `Authorization` header, so the `/ws` upgrade accepts the token here as well.
    /// Security: tokens in URLs can leak into server logs and browser history — acceptable
    /// for the loopback/dev posture of sub-project A; the header path stays the recommended
    /// one for non-browser clients. A later sub-project may move this to a WS subprotocol
    /// carrier or route through Tauri's Rust-side WS client (which can set headers).
    token: Option<String>,
}

/// Shared server state: the engine service and the required bearer token.
struct ServeState {
    service: EngineService,
    token: String,
    capabilities: CapabilitiesManifest,
    /// `Some` when `--promote-loopback`/`--promote-vps` is set; enables the handover commands.
    promote: Option<PromoteConfig>,
    /// `true` when `--accept-promotions` is set; enables the inbound `POST /promote` restore RPC.
    accept_promotions: bool,
    /// This server's own public ws base (e.g. `ws://host:port`), reported as the reconnect
    /// target on a vps `Demoted`. `Some(base)` when this serve can be demoted-back-to; `None`
    /// for servers never demoted-from.
    public_ws_base: Option<String>,
    /// Provisioned engines, retained so they outlive the local connection that created them
    /// (a dropped `RemoteHandle` aborts its engine task). Keyed by `(session, to_remote)` so a
    /// cache hit always corresponds to the same direction and the reply label cannot be mislabelled
    /// by a malformed client sequence that flips the direction on a repeat call.
    remotes: Mutex<HashMap<(SessionId, bool), RemoteHandle>>,
}

/// True if `headers` carry `Authorization: Bearer <token>` matching `token`.
fn authorized(headers: &HeaderMap, token: &str) -> bool {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        == Some(token)
}

/// True if the `/ws` upgrade is authorized: a matching `Authorization: Bearer` header
/// (preferred) or a matching `?token=` query param (the browser path).
fn authorized_ws(headers: &HeaderMap, token: &str, params: &ConnectParams) -> bool {
    authorized(headers, token) || params.token.as_deref() == Some(token)
}

/// Resolve the TLS flag pair: both present -> `Some((cert, key))`; neither -> `None`;
/// exactly one -> error (the flags must be given together).
pub fn resolve_tls_paths(
    cert: Option<PathBuf>,
    key: Option<PathBuf>,
) -> anyhow::Result<Option<(PathBuf, PathBuf)>> {
    match (cert, key) {
        (Some(c), Some(k)) => Ok(Some((c, k))),
        (None, None) => Ok(None),
        _ => anyhow::bail!("--tls-cert and --tls-key must be provided together"),
    }
}

/// Build the axum app. Exposed for tests so they can serve it on an ephemeral port.
pub fn app(
    service: EngineService,
    token: String,
    capabilities: CapabilitiesManifest,
    promote: Option<PromoteConfig>,
    accept_promotions: bool,
) -> AxumRouter {
    app_inner(
        service,
        token,
        capabilities,
        promote,
        accept_promotions,
        None,
    )
}

/// Build the axum app, specifying this server's own public ws base (the vps-demote reconnect
/// target). `app` is the same with no base, for servers that are never demoted-from.
pub fn app_with_base(
    service: EngineService,
    token: String,
    capabilities: CapabilitiesManifest,
    promote: Option<PromoteConfig>,
    accept_promotions: bool,
    public_ws_base: String,
) -> AxumRouter {
    app_inner(
        service,
        token,
        capabilities,
        promote,
        accept_promotions,
        Some(public_ws_base),
    )
}

fn app_inner(
    service: EngineService,
    token: String,
    capabilities: CapabilitiesManifest,
    promote: Option<PromoteConfig>,
    accept_promotions: bool,
    public_ws_base: Option<String>,
) -> AxumRouter {
    assert!(!token.is_empty(), "serve token must not be empty");
    let state = Arc::new(ServeState {
        service,
        token,
        capabilities,
        promote,
        accept_promotions,
        public_ws_base,
        remotes: Mutex::new(HashMap::new()),
    });
    // CORS for the browser UI: it is served from a different origin (trunk on :8080) and its
    // POST /workspace carries an `Authorization` header, so the browser sends a preflight.
    // `allow_origin(Any)` matches the loopback/dev posture already accepted for the `?token=`
    // query param on /ws; auth rides the Authorization header (not cookies), so wildcard origin
    // without credentials mode is correct and exposes nothing extra.
    // SECURITY: never add `.allow_credentials(true)` here — with `allow_origin(Any)` that is
    // both a tower-http startup panic and a real cross-origin credential leak.
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE]);
    AxumRouter::new()
        .route("/ws", get(ws_handler))
        .route("/workspace", post(workspace_handler))
        .route("/promote", post(promote_handler))
        .route("/export", post(export_handler))
        .layer(cors)
        .with_state(state)
}

/// Serve `app` on a pre-bound listener, over TLS when `tls` is `Some`. Unifies the plaintext
/// and TLS paths on `axum-server` so both run from a `std::net::TcpListener` (testable on a
/// `127.0.0.1:0` ephemeral port). The listener must be in non-blocking mode.
pub async fn run(
    listener: std::net::TcpListener,
    app: AxumRouter,
    tls: Option<RustlsConfig>,
) -> anyhow::Result<()> {
    match tls {
        Some(cfg) => {
            axum_server::from_tcp_rustls(listener, cfg)
                .serve(app.into_make_service())
                .await?
        }
        None => {
            axum_server::from_tcp(listener)
                .serve(app.into_make_service())
                .await?
        }
    }
    Ok(())
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    headers: HeaderMap,
    Query(params): Query<ConnectParams>,
    State(state): State<Arc<ServeState>>,
) -> Response {
    if !authorized_ws(&headers, &state.token, &params) {
        return (StatusCode::UNAUTHORIZED, "missing or invalid bearer token").into_response();
    }
    ws.on_upgrade(move |socket| handle_socket(socket, params, state))
}

async fn workspace_handler(
    headers: HeaderMap,
    State(state): State<Arc<ServeState>>,
    body: axum::body::Bytes,
) -> Response {
    if !authorized(&headers, &state.token) {
        return (StatusCode::UNAUTHORIZED, "missing or invalid bearer token").into_response();
    }
    let req: WorkspaceRequest = match serde_json::from_slice(&body) {
        Ok(req) => req,
        Err(e) => return (StatusCode::BAD_REQUEST, format!("bad request: {e}")).into_response(),
    };
    let resp = state.service.workspace_rpc(req).await;
    axum::Json(resp).into_response()
}

/// Inbound restore RPC (receiver role). Fail-closed: `403` unless `--accept-promotions`, `401`
/// without a valid bearer. Restores the bundle into this engine's store + workspace (gated).
async fn promote_handler(
    headers: HeaderMap,
    State(state): State<Arc<ServeState>>,
    body: axum::body::Bytes,
) -> Response {
    if !state.accept_promotions {
        return (StatusCode::FORBIDDEN, "promotion acceptance disabled").into_response();
    }
    if !authorized(&headers, &state.token) {
        return (StatusCode::UNAUTHORIZED, "missing or invalid bearer token").into_response();
    }
    let bundle: otto_remote::PromoteBundle = match serde_json::from_slice(&body) {
        Ok(b) => b,
        Err(e) => return (StatusCode::BAD_REQUEST, format!("bad request: {e}")).into_response(),
    };
    match state.service.accept_promotion(&bundle).await {
        Ok(session) => {
            axum::Json(serde_json::json!({ "session": session.0.to_string() })).into_response()
        }
        Err(crate::service::AcceptError::AlreadyExists) => {
            (StatusCode::CONFLICT, "session already exists").into_response()
        }
        Err(crate::service::AcceptError::Refused(msg)) => {
            // Hostile/malformed bundle — a client fault, not a receiver failure.
            (StatusCode::BAD_REQUEST, msg).into_response()
        }
        Err(crate::service::AcceptError::Failed(e)) => {
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
    }
}

/// Outbound export RPC (receiver role): returns a session's `PromoteBundle` so a demoting source
/// can pull it back. Same gate as `/promote`: `403` unless `--accept-promotions`, `401` without a
/// valid bearer. The bundle's workspace snapshot is gate-filtered (secrets never leave here).
async fn export_handler(
    headers: HeaderMap,
    State(state): State<Arc<ServeState>>,
    body: axum::body::Bytes,
) -> Response {
    if !state.accept_promotions {
        return (StatusCode::FORBIDDEN, "promotion acceptance disabled").into_response();
    }
    if !authorized(&headers, &state.token) {
        return (StatusCode::UNAUTHORIZED, "missing or invalid bearer token").into_response();
    }
    #[derive(serde::Deserialize)]
    struct ExportRequest {
        session: String,
    }
    let req: ExportRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => return (StatusCode::BAD_REQUEST, format!("bad request: {e}")).into_response(),
    };
    let session = match uuid::Uuid::parse_str(&req.session) {
        Ok(u) => SessionId(u),
        Err(e) => return (StatusCode::BAD_REQUEST, format!("bad session id: {e}")).into_response(),
    };
    match state.service.export_promotion(session).await {
        Ok(bundle) => axum::Json(bundle).into_response(),
        // Unknown session (snapshot errors when the row is absent) → 404, not a 500.
        Err(_) => (StatusCode::NOT_FOUND, "unknown session").into_response(),
    }
}

/// The writer half of a split WebSocket — what events and frames are sent through.
type WsWriter = SplitSink<WebSocket, Message>;

/// Per-connection registry of pending edit approvals, keyed by the `ApprovalRequest` id.
/// Shared between the running turn's `InteractiveApprover` and the socket-reader that routes
/// inbound `ApproveDiff` frames. Dropping a sender (on `clear`/disconnect) resolves the awaiting
/// `request()` to `false` — the single fail-closed rule.
#[derive(Clone, Default)]
struct ApprovalRegistry {
    pending: Arc<Mutex<HashMap<Uuid, oneshot::Sender<bool>>>>,
}

impl ApprovalRegistry {
    fn new() -> Self {
        Self::default()
    }

    fn insert(&self, id: Uuid, tx: oneshot::Sender<bool>) {
        self.pending.lock().unwrap().insert(id, tx);
    }

    fn resolve(&self, id: Uuid, approved: bool) {
        if let Some(tx) = self.pending.lock().unwrap().remove(&id) {
            let _ = tx.send(approved);
        }
    }

    /// Drop all pending senders → every awaiting `request()` resolves `false` (fail-closed).
    fn clear(&self) {
        self.pending.lock().unwrap().clear();
    }
}

/// Approver that surfaces each request to the connected UI and awaits its `ApproveDiff` reply.
struct InteractiveApprover {
    registry: ApprovalRegistry,
}

impl InteractiveApprover {
    fn new(registry: ApprovalRegistry) -> Self {
        Self { registry }
    }
}

#[async_trait::async_trait]
impl Approver for InteractiveApprover {
    async fn request(&self, id: Uuid, _path: &Path, _old: Option<&str>, _new: &str) -> bool {
        let (tx, rx) = oneshot::channel();
        self.registry.insert(id, tx);
        // A closed channel (disconnect / clear) → reject. Fail-closed.
        rx.await.unwrap_or(false)
    }
}

/// Connection-scoped pause state: a flag plus a notify to wake parked turns. Shared between the
/// running turn's `InteractivePauser` and the socket reader that routes `Pause`/`Resume`.
#[derive(Default)]
struct PauseState {
    paused: AtomicBool,
    resume: Notify,
}

impl PauseState {
    fn pause(&self) {
        self.paused.store(true, Ordering::SeqCst);
    }
    /// Clear the flag and wake any parked turn (also the disconnect/abort release path).
    fn resume_all(&self) {
        self.paused.store(false, Ordering::SeqCst);
        self.resume.notify_waiters();
    }
}

/// Pause controller backed by a connection's `PauseState`.
struct InteractivePauser(Arc<PauseState>);

#[async_trait::async_trait]
impl PauseController for InteractivePauser {
    fn should_pause(&self) -> bool {
        self.0.paused.load(Ordering::SeqCst)
    }
    async fn wait_for_resume(&self) {
        loop {
            // Arm the notified future BEFORE re-checking the flag, so a Resume that fires between
            // the check and the await is not lost. `notified()` alone does not enqueue the waiter
            // until first polled, so `enable()` registers it now — otherwise a `notify_waiters()`
            // landing in this window would wake nothing and the turn would park forever.
            let notified = self.0.resume.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if !self.0.paused.load(Ordering::SeqCst) {
                return;
            }
            notified.await;
        }
    }
}

/// Send one `ServerMessage` as a JSON text frame through the writer half.
async fn send_msg(writer: &mut WsWriter, msg: &ServerMessage) -> anyhow::Result<()> {
    let json = serde_json::to_string(msg)?;
    writer.send(Message::Text(json.into())).await?;
    Ok(())
}

/// A sink that writes each event to the socket's writer half as a `ServerMessage::Event` frame.
struct WsSink<'a> {
    writer: &'a mut WsWriter,
}

#[async_trait::async_trait]
impl EventSink for WsSink<'_> {
    async fn emit(&mut self, event: &Event) -> anyhow::Result<()> {
        send_msg(
            self.writer,
            &ServerMessage::Event {
                event: event.clone(),
            },
        )
        .await
    }
}

async fn handle_socket(socket: WebSocket, params: ConnectParams, state: Arc<ServeState>) {
    // Split up-front so the turn (writer) and inbound approvals (reader) can run concurrently.
    let (mut writer, mut reader) = socket.split();

    let session = match resolve_session(&params, &state).await {
        Ok(s) => s,
        Err(e) => {
            let _ = send_msg(
                &mut writer,
                &ServerMessage::Error {
                    message: e.to_string(),
                },
            )
            .await;
            return;
        }
    };

    if send_msg(
        &mut writer,
        &ServerMessage::Ready {
            session,
            capabilities: state.capabilities.clone(),
        },
    )
    .await
    .is_err()
    {
        return;
    }

    // Reconnect: replay the gap after `last_seq`.
    if let Some(after) = params.last_seq {
        match state
            .service
            .store()
            .replay_since(session, Some(after))
            .await
        {
            Ok(events) => {
                for event in events {
                    if send_msg(&mut writer, &ServerMessage::Event { event })
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
            }
            Err(e) => {
                let _ = send_msg(
                    &mut writer,
                    &ServerMessage::Error {
                        message: e.to_string(),
                    },
                )
                .await;
                return;
            }
        }
    }

    let approvals = ApprovalRegistry::new();
    let pause_state = Arc::new(PauseState::default());

    'outer: while let Some(Ok(msg)) = reader.next().await {
        let text = match msg {
            Message::Text(t) => t,
            Message::Close(_) => break,
            _ => continue, // ignore binary/ping/pong
        };
        let command: Command = match serde_json::from_str(text.as_str()) {
            Ok(c) => c,
            Err(e) => {
                let _ = send_msg(
                    &mut writer,
                    &ServerMessage::Error {
                        message: format!("bad command: {e}"),
                    },
                )
                .await;
                continue;
            }
        };
        match command {
            Command::SendPrompt { text, .. } => {
                let approver = Arc::new(InteractiveApprover::new(approvals.clone()));
                let pauser = Arc::new(InteractivePauser(Arc::clone(&pause_state)));
                let controls = TurnControls {
                    approver,
                    pauser,
                    tools: None,
                    router: None,
                };
                // Drive the turn while concurrently reading inbound approvals. The turn borrows
                // `writer` (via the sink); the reader borrows `reader` — disjoint, so `select!`
                // can poll both. `StreamExt::next` is cancel-safe, so the reader future being
                // dropped when the turn wins a race loses no inbound frame.
                let turn_err = {
                    let mut sink = WsSink {
                        writer: &mut writer,
                    };
                    let turn = state
                        .service
                        .run_prompt_with_controls(session, &text, &mut sink, controls);
                    tokio::pin!(turn);
                    let mut err: Option<anyhow::Error> = None;
                    loop {
                        tokio::select! {
                            res = &mut turn => {
                                if let Err(e) = res {
                                    err = Some(e);
                                }
                                approvals.clear();
                                // Drop any leftover pause flag so a Pause that arrived but was
                                // never resumed before the turn ended cannot pre-pause the next one.
                                pause_state.resume_all();
                                break;
                            }
                            inbound = reader.next() => match inbound {
                                Some(Ok(Message::Text(t))) => {
                                    match serde_json::from_str::<Command>(t.as_str()) {
                                        Ok(Command::ApproveDiff { id, approved, .. }) => {
                                            approvals.resolve(id, approved);
                                        }
                                        Ok(Command::Pause { .. }) => {
                                            pause_state.pause();
                                        }
                                        Ok(Command::Resume { .. }) => {
                                            pause_state.resume_all();
                                        }
                                        Ok(Command::Abort { .. }) => {
                                            let _ = state.service.abort(session).await;
                                            approvals.clear();
                                            pause_state.resume_all();
                                            break 'outer;
                                        }
                                        // A second SendPrompt mid-turn is ignored (one turn at a
                                        // time); other commands are no-ops here.
                                        _ => {}
                                    }
                                }
                                Some(Ok(Message::Close(_))) | None => {
                                    approvals.clear();
                                    pause_state.resume_all();
                                    break 'outer;
                                }
                                _ => {}
                            }
                        }
                    }
                    err
                }; // `sink` dropped here → `writer` is free again

                if let Some(e) = turn_err {
                    let _ = send_msg(
                        &mut writer,
                        &ServerMessage::Error {
                            message: e.to_string(),
                        },
                    )
                    .await;
                }
            }
            Command::Abort { .. } => {
                let _ = state.service.abort(session).await;
                break;
            }
            Command::ApproveDiff { .. } => {
                // No turn in flight: a stray approval is ignored.
            }
            Command::CreateSession => {
                // The session is already established on connect; nothing to do.
            }
            Command::Pause { .. } => {
                pause_state.pause();
            }
            Command::Resume { .. } => {
                pause_state.resume_all();
            }
            Command::PromoteToRemote { .. } => {
                handle_handover(&state, &mut writer, session, true).await;
            }
            Command::DemoteToLocal { .. } => {
                handle_handover(&state, &mut writer, session, false).await;
            }
            // TODO(serve-run-command Task 5): wire this to `EngineService`'s command-lookup +
            // narrowed-registry/pinned-router turn. This stub only keeps `match command`
            // exhaustive after the protocol variant landed (Task 1) — no turn starts, no `seq`
            // is consumed, matching the variant's documented not-yet-wired posture.
            Command::RunCommand { .. } => {
                let _ = send_msg(
                    &mut writer,
                    &ServerMessage::Error {
                        message: "RunCommand is not yet supported on this server".to_string(),
                    },
                )
                .await;
            }
        }
    }
}

/// Provision the session onto a fresh engine (remote for promote, local for demote), retain the
/// handle so it outlives this connection, and tell the client where to reconnect. A no-op error
/// reply when promotion is not enabled (the promotion-disabled posture). Handled between turns.
async fn handle_handover(
    state: &ServeState,
    writer: &mut WsWriter,
    session: SessionId,
    to_remote: bool,
) {
    let Some(cfg) = state.promote.as_ref() else {
        let _ = send_msg(
            writer,
            &ServerMessage::Error {
                message:
                    "remote provisioning unavailable (start otto serve with --promote-loopback or --promote-vps)"
                        .to_string(),
            },
        )
        .await;
        return;
    };

    // Vps demote: pull the session's current bundle back off the receiver and restore it into THIS
    // (source) engine, overwriting our own stale copy. Symmetric inverse of the promote push. The
    // client reconnects to us (the session is local again), so we report our own public ws base.
    if !to_remote {
        if let otto_remote::PromoteMode::Vps { endpoint } = &cfg.mode {
            let target = otto_remote::VpsTarget::new(endpoint.clone(), cfg.token.clone());
            let bundle = match target.export(session).await {
                Ok(b) => b,
                Err(e) => {
                    let _ = send_msg(
                        writer,
                        &ServerMessage::Error {
                            message: e.to_string(),
                        },
                    )
                    .await;
                    return;
                }
            };
            if let Err(e) = state.service.accept_demotion(&bundle).await {
                let msg = match e {
                    crate::service::AcceptError::Refused(m) => m,
                    crate::service::AcceptError::Failed(err) => err.to_string(),
                    // unreachable: accept_demotion uses restore_over (overwrite), never AlreadyExists
                    crate::service::AcceptError::AlreadyExists => {
                        "demote restore conflict".to_string()
                    }
                };
                let _ = send_msg(writer, &ServerMessage::Error { message: msg }).await;
                return;
            }
            match &state.public_ws_base {
                Some(base) => {
                    let _ = send_msg(
                        writer,
                        &ServerMessage::Demoted {
                            session,
                            endpoint: base.clone(),
                        },
                    )
                    .await;
                }
                None => {
                    // Misconfiguration: a vps-demotable serve must be built via serve_app_with_base.
                    let _ = send_msg(
                        writer,
                        &ServerMessage::Error {
                            message: "demote target has no public ws base configured".to_string(),
                        },
                    )
                    .await;
                }
            }
            return;
        }
        if let otto_remote::PromoteMode::Microvm { .. } = &cfg.mode {
            // Source the live microVM endpoint+token from the handle a prior promote stored under
            // (session, true). No handle ⟹ nothing to pull from. Take the lock only to clone the
            // endpoint/token, releasing it at the `;` before any await.
            let live = state
                .remotes
                .lock()
                .unwrap()
                .get(&(session, true))
                .map(|h| (h.endpoint.clone(), h.token.clone()));
            let Some((endpoint, token)) = live else {
                let _ = send_msg(
                    writer,
                    &ServerMessage::Error {
                        message: "no active microvm handover for this session; promote first"
                            .to_string(),
                    },
                )
                .await;
                return;
            };

            // Pull the current bundle off the microVM. On failure, leave the VM running (a transient
            // pull error must not lose the session) and report the error.
            let bundle = match otto_remote::export_bundle(&endpoint, &token, session).await {
                Ok(b) => b,
                Err(e) => {
                    let _ = send_msg(
                        writer,
                        &ServerMessage::Error {
                            message: e.to_string(),
                        },
                    )
                    .await;
                    return;
                }
            };

            // Restore into THIS engine, overwriting our stale pre-promote copy (fail-closed
            // sensitive-path floor first). On failure, leave the VM running and report.
            if let Err(e) = state.service.accept_demotion(&bundle).await {
                let msg = match e {
                    crate::service::AcceptError::Refused(m) => m,
                    crate::service::AcceptError::Failed(err) => err.to_string(),
                    // unreachable: accept_demotion uses restore_over (overwrite), never AlreadyExists
                    crate::service::AcceptError::AlreadyExists => {
                        "demote restore conflict".to_string()
                    }
                };
                let _ = send_msg(writer, &ServerMessage::Error { message: msg }).await;
                return;
            }

            // Success only: drop the handle to dispose the microVM, then tell the client to
            // reconnect to us (the session is local again).
            state.remotes.lock().unwrap().remove(&(session, true));
            match &state.public_ws_base {
                Some(base) => {
                    let _ = send_msg(
                        writer,
                        &ServerMessage::Demoted {
                            session,
                            endpoint: base.clone(),
                        },
                    )
                    .await;
                }
                None => {
                    // Misconfiguration: a demotable serve must be built via serve_app_with_base.
                    // The session is already local (restore committed) and the VM disposed; this
                    // only signals the operator's missing public ws base.
                    let _ = send_msg(
                        writer,
                        &ServerMessage::Error {
                            message: "demote target has no public ws base configured".to_string(),
                        },
                    )
                    .await;
                }
            }
            return;
        }
    }

    // Reuse an existing handover for this session (idempotent): provisioning again would drop the
    // prior RemoteHandle and abort an engine a client may still be connected to. Bind the lookup
    // to a local so the Mutex guard is released at the `;` — never held across the await below.
    let existing = state
        .remotes
        .lock()
        .unwrap()
        .get(&(session, to_remote))
        .map(|h| h.endpoint.clone());
    let endpoint = match existing {
        Some(endpoint) => endpoint,
        None => {
            let target: Box<dyn otto_remote::RemoteTarget> =
                match &cfg.mode {
                    otto_remote::PromoteMode::Loopback { base_dir } => Box::new(
                        LoopbackTarget::new(cfg.token.clone(), base_dir.clone(), to_remote),
                    ),
                    otto_remote::PromoteMode::Vps { endpoint } => Box::new(
                        otto_remote::VpsTarget::new(endpoint.clone(), cfg.token.clone()),
                    ),
                    otto_remote::PromoteMode::Microvm { config } => {
                        #[cfg(feature = "firecracker")]
                        let provisioner: std::sync::Arc<
                            dyn otto_remote::Provisioner,
                        > = std::sync::Arc::new(otto_remote::FirecrackerProvisioner::new(
                            config.clone(),
                            cfg.token.clone(),
                        ));
                        #[cfg(not(feature = "firecracker"))]
                        let provisioner: std::sync::Arc<
                            dyn otto_remote::Provisioner,
                        > = {
                            let _ = config; // unused without the firecracker feature
                            std::sync::Arc::new(otto_remote::UnsupportedProvisioner)
                        };
                        Box::new(otto_remote::MicrovmTarget::new(provisioner))
                    }
                };
            let handle = match promote(
                state.service.store(),
                state.service.workspace(),
                session,
                &*target,
            )
            .await
            {
                Ok(h) => h,
                Err(e) => {
                    let _ = send_msg(
                        writer,
                        &ServerMessage::Error {
                            message: e.to_string(),
                        },
                    )
                    .await;
                    return;
                }
            };
            let endpoint = handle.endpoint.clone();
            // Retain BEFORE replying: for loopback, dropping the handle aborts the provisioned
            // engine; for vps the handle's shutdown is None, so retention is cheap and harmless.
            state
                .remotes
                .lock()
                .unwrap()
                .insert((session, to_remote), handle);
            endpoint
        }
    };
    let msg = if to_remote {
        ServerMessage::Promoted { session, endpoint }
    } else {
        ServerMessage::Demoted { session, endpoint }
    };
    let _ = send_msg(writer, &msg).await;
}

async fn resolve_session(params: &ConnectParams, state: &ServeState) -> anyhow::Result<SessionId> {
    match &params.session {
        Some(s) => {
            let uuid = uuid::Uuid::parse_str(s)?;
            Ok(SessionId(uuid))
        }
        None => {
            state
                .service
                .create_session("(serve/ws)", &serde_json::json!({}))
                .await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn tls_paths_both_present_is_some() {
        let got =
            resolve_tls_paths(Some(PathBuf::from("c.pem")), Some(PathBuf::from("k.pem"))).unwrap();
        assert_eq!(got, Some((PathBuf::from("c.pem"), PathBuf::from("k.pem"))));
    }

    #[test]
    fn tls_paths_neither_is_none() {
        assert_eq!(resolve_tls_paths(None, None).unwrap(), None);
    }

    #[test]
    fn tls_paths_only_one_is_error() {
        assert!(resolve_tls_paths(Some(PathBuf::from("c.pem")), None).is_err());
        assert!(resolve_tls_paths(None, Some(PathBuf::from("k.pem"))).is_err());
    }
}
