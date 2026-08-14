//! WebSocket transport for the engine. Maps WS frames to `Command`/event frames over an
//! `EngineService`: a `Hello { auth_mode }` greeting on upgrade, then per-mode authentication
//! (the `Login`/`Attach` handshake under a deadline in `Users` and `Machine`, nothing in
//! `SingleUser`), a `Ready { session }` frame once a principal owns a session,
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
use futures_util::stream::{SplitSink, SplitStream, StreamExt};
use otto_engine_core::TurnOutcome;
use otto_engine_core::auth::{AuthConfig, Authenticator, TokenPair};
use otto_engine_core::tool::Approver;
use otto_engine_core::tool::PauseController;
use otto_protocol::{
    AuthMode, CapabilitiesManifest, Command, Event, ServerMessage, SessionId, UserId,
    WorkspaceRequest,
};
use serde::Deserialize;
use subtle::ConstantTimeEq;
use tokio::sync::Notify;
use tokio::sync::oneshot;
use tower_http::cors::{Any, CorsLayer};
use tower_http::services::{ServeDir, ServeFile};
use uuid::Uuid;

use crate::loopback::LoopbackTarget;
use crate::service::{EngineService, EventSink, TurnControls};
use otto_remote::{PromoteConfig, RemoteHandle, RemoteTarget, promote};

#[derive(Deserialize, Default)]
struct ConnectParams {
    session: Option<String>,
    last_seq: Option<u64>,
}

/// The single opaque string every failed authentication presents to the client (A11): the real
/// cause — wrong code, unknown user, replayed step, expired/denylisted token, a non-auth command
/// before authentication, a missed deadline — is logged server-side only.
const AUTH_FAILED: &str = "authentication failed";

/// Shared server state: the engine service and the auth posture (`AuthConfig`), plus the
/// handover plumbing.
struct ServeState {
    service: EngineService,
    auth: AuthConfig,
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
    /// The receiver's session→secret map (spec §3.1): the per-session secret recorded on a
    /// successful `/promote` push, consulted by `/export`, the `Machine`-mode WS handshake, and
    /// `/workspace` (membership, A6), and disposed when the session is demoted. Transient
    /// transport state, never persisted and never logged; no `Debug` impl formats it.
    session_secrets: Mutex<HashMap<SessionId, String>>,
}

impl ServeState {
    /// The configured authenticator — present exactly when the mode is `Users`.
    fn authenticator(&self) -> Option<&Arc<dyn Authenticator>> {
        self.auth.authenticator.as_ref()
    }

    /// Record the per-session secret carried on a successful `/promote` push.
    fn record_session_secret(&self, session: SessionId, secret: String) {
        self.session_secrets.lock().unwrap().insert(session, secret);
    }

    /// The recorded per-session secret for `session`, if any. Cloned out under the lock so the
    /// guard is released before any await by the caller.
    fn session_secret(&self, session: SessionId) -> Option<String> {
        self.session_secrets.lock().unwrap().get(&session).cloned()
    }

    /// Consume a session's secret after a successful `/export`: demote consumes the credential,
    /// so a disposed secret is indistinguishable from one never recorded (both `None`).
    fn dispose_session_secret(&self, session: SessionId) {
        self.session_secrets.lock().unwrap().remove(&session);
    }

    /// `/workspace` authorization on a `Machine` receiver (A6): the machine secret (the
    /// operator/back-compat path) OR membership in the session→secret map — each entry compared
    /// constant-time. The RPC is machine-scoped with no session id in the request, so checking
    /// against every live session secret is the session-side credential check.
    fn machine_workspace_authorized(&self, headers: &HeaderMap) -> bool {
        if authorized(headers, self.auth.promotion_secret.as_deref()) {
            return true;
        }
        let Some(provided) = bearer_token(headers) else {
            return false;
        };
        let secrets = self.session_secrets.lock().unwrap();
        secrets
            .values()
            .any(|s| bool::from(s.as_bytes().ct_eq(provided.as_bytes())))
    }
}

/// True if `headers` carry `Authorization: Bearer <secret>` — compared constant-time via
/// `subtle`, so a wrong-length or wrong-value secret leaks neither its prefix nor its length
/// through a timing side channel.
fn authorized(headers: &HeaderMap, secret: Option<&str>) -> bool {
    match (secret, bearer_token(headers)) {
        (Some(secret), Some(provided)) => bool::from(secret.as_bytes().ct_eq(provided.as_bytes())),
        _ => false,
    }
}

/// The bearer token from an `Authorization: Bearer` header, if present.
fn bearer_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|t| t.to_string())
}

/// Constant-time compare of a frame-carried credential (an `Attach` token) against the promotion
/// secret. The header path is `authorized`; this is the post-upgrade sibling for the `Machine`
/// mode's one frame.
fn secret_matches(secret: Option<&str>, provided: &str) -> bool {
    match secret {
        Some(secret) => bool::from(secret.as_bytes().ct_eq(provided.as_bytes())),
        None => false,
    }
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
    auth: AuthConfig,
    capabilities: CapabilitiesManifest,
    promote: Option<PromoteConfig>,
    accept_promotions: bool,
) -> AxumRouter {
    app_inner(
        service,
        auth,
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
    auth: AuthConfig,
    capabilities: CapabilitiesManifest,
    promote: Option<PromoteConfig>,
    accept_promotions: bool,
    public_ws_base: String,
) -> AxumRouter {
    app_inner(
        service,
        auth,
        capabilities,
        promote,
        accept_promotions,
        Some(public_ws_base),
    )
}

/// Serve a pre-built web UI bundle from `dir` as this router's fallback.
///
/// Applied *after* construction rather than threaded through `app`/`app_with_base`. Those two
/// already differ only by one optional argument; a `ui_dir` parameter would make four
/// constructors and churn all six call sites. This layer changes none of them.
///
/// ## Two security invariants, both deliberate
///
/// 1. **This route is unauthenticated on purpose.** A browser must fetch `index.html` and the
///    wasm *before* it has a token to present, so requiring a bearer here would break first
///    load. It is safe because the bundle is public build output: every path that carries
///    session data or workspace contents — `/ws`, `/workspace`, `/promote`, `/export` — keeps
///    its own bearer check, unchanged. Do not "fix" this by adding auth.
///
/// 2. **Nothing reachable from `dir` — including through symlinks — may lie outside it.**
///    `ServeDir` does *not* consult the permission gate's sensitive-path floor, and it does
///    *not* canonicalize the resolved path or verify it stays under `dir`: it only rejects
///    `..`/root/prefix path *components* in the request itself (confirmed: `/../../etc/passwd`
///    and both its percent-encoded forms are rejected). A symlink placed *inside* `dir` that
///    points outside it is followed and served — reproduced end-to-end (`bundle/link-to-env` ->
///    `<workspace>/.env` served the secret over plain HTTP). `dx build` output contains no
///    symlinks today, so this is not exploitable through the shipped pipeline, but it means
///    `dir` must be build output only, never a directory a running session (or anything else)
///    can write into — pointing this at (or nesting it under) a workspace root would serve
///    `.env`, `.ssh/`, and `.git/`, bypassing the single most important invariant in the
///    codebase. `dir` is operator-supplied via `--ui-dir` (validated to exist and be a directory
///    before this is ever called — see `validate_ui_dir` in `main.rs`), has no default and no env
///    fallback, and when the flag is absent this layer is never applied and the route does not
///    exist.
pub fn with_ui_dir(app: AxumRouter, dir: PathBuf) -> AxumRouter {
    let index = dir.join("index.html");
    app.fallback_service(ServeDir::new(dir).fallback(ServeFile::new(index)))
}

fn app_inner(
    service: EngineService,
    auth: AuthConfig,
    capabilities: CapabilitiesManifest,
    promote: Option<PromoteConfig>,
    accept_promotions: bool,
    public_ws_base: Option<String>,
) -> AxumRouter {
    let state = Arc::new(ServeState {
        service,
        auth,
        capabilities,
        promote,
        accept_promotions,
        public_ws_base,
        remotes: Mutex::new(HashMap::new()),
        session_secrets: Mutex::new(HashMap::new()),
    });
    // CORS for the browser UI: it is served from a different origin (dx dev server or
    // otto serve --ui-dir) and its POST /workspace carries an `Authorization` header,
    // so the browser sends a preflight.
    // `allow_origin(Any)` matches the loopback/dev posture already accepted for the post-upgrade
    // `Login`/`Attach` handshake on /ws; auth rides the Authorization header or a WS frame (not
    // cookies), so wildcard origin without credentials mode is correct and exposes nothing extra.
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

/// The `/ws` upgrade itself is not the auth boundary (spec §7.2): the socket is accepted and the
/// `Hello` greeting + `Login`/`Attach` handshake run inside `handle_socket`, per the mode. The
/// `Authorization` header is passed through so a non-browser client can pre-resolve a principal.
async fn ws_handler(
    ws: WebSocketUpgrade,
    headers: HeaderMap,
    Query(params): Query<ConnectParams>,
    State(state): State<Arc<ServeState>>,
) -> Response {
    ws.on_upgrade(move |socket| handle_socket(socket, headers, params, state))
}

/// `POST /workspace` — the per-mode credential table (spec §7.3): `SingleUser` ignores the header
/// entirely (the route is loopback-bound, single-principal, mints nothing); `Users` requires a
/// valid access token, verified exactly like the WS path; `Machine` requires the promotion
/// secret, constant-time. The workspace itself is *not* per-tenant isolated (one process-global
/// root) — authentication only, per the design spec.
async fn workspace_handler(
    headers: HeaderMap,
    State(state): State<Arc<ServeState>>,
    body: axum::body::Bytes,
) -> Response {
    match state.auth.mode {
        AuthMode::SingleUser => {}
        AuthMode::Users => {
            let Some(token) = bearer_token(&headers) else {
                return (StatusCode::UNAUTHORIZED, "missing or invalid bearer token")
                    .into_response();
            };
            let Some(authenticator) = state.authenticator() else {
                return (StatusCode::UNAUTHORIZED, "missing or invalid bearer token")
                    .into_response();
            };
            if let Err(e) = authenticator.verify_access(&token).await {
                eprintln!("serve: /workspace rejected: {e:?}");
                return (StatusCode::UNAUTHORIZED, "missing or invalid bearer token")
                    .into_response();
            }
        }
        AuthMode::Machine => {
            // A6: the machine secret (operator path) OR any live session secret (a promoted
            // client reconnects with its per-session secret and drives /workspace with it).
            if !state.machine_workspace_authorized(&headers) {
                return (StatusCode::UNAUTHORIZED, "missing or invalid bearer token")
                    .into_response();
            }
        }
    }
    let req: WorkspaceRequest = match serde_json::from_slice(&body) {
        Ok(req) => req,
        Err(e) => return (StatusCode::BAD_REQUEST, format!("bad request: {e}")).into_response(),
    };
    let resp = state.service.workspace_rpc(req).await;
    axum::Json(resp).into_response()
}

/// Detect the pre-ownership bundle shape that `/promote` special-cases (spec §3.2 / premise
/// correction 1): a `PromoteBundle` whose top-level `session` object is present but lacks the
/// `owner` key — a bundle serialized before slice 1a's session ownership. Returns `true` only for
/// that exact shape. A body that is not JSON, has no `session` object, or whose `session` is not an
/// object keeps the ordinary `bad request: {e}` 400.
fn is_pre_ownership_bundle(body: &[u8]) -> bool {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(body) else {
        return false;
    };
    match value.get("session") {
        Some(serde_json::Value::Object(session)) => !session.contains_key("owner"),
        _ => false,
    }
}

/// Inbound restore RPC (receiver role). Fail-closed: `403` unless `--accept-promotions`, `401`
/// without the promotion secret (constant-time). Restores the bundle into this engine's store +
/// workspace (gated). A pre-ownership bundle (a `session` lacking `owner`) gets the actionable
/// 400; the `X-Otto-Session-Secret` header is required (A2) and recorded for `/export`/WS attach.
async fn promote_handler(
    headers: HeaderMap,
    State(state): State<Arc<ServeState>>,
    body: axum::body::Bytes,
) -> Response {
    if !state.accept_promotions {
        return (StatusCode::FORBIDDEN, "promotion acceptance disabled").into_response();
    }
    if !authorized(&headers, state.auth.promotion_secret.as_deref()) {
        return (StatusCode::UNAUTHORIZED, "missing or invalid bearer token").into_response();
    }
    let bundle: otto_remote::PromoteBundle = match serde_json::from_slice(&body) {
        Ok(b) => b,
        Err(e) => {
            if is_pre_ownership_bundle(&body) {
                // The legacy break inside `SessionState`'s deserialization (missing `owner`),
                // with the operator-actionable message (slice 1a's §4.1, adapted to the wire).
                return (
                    StatusCode::BAD_REQUEST,
                    "promote bundle predates session ownership (issue #115): its session carries \
                     no owner. otto has no installed base, so there is no migration — re-promote \
                     from a current otto.",
                )
                    .into_response();
            }
            return (StatusCode::BAD_REQUEST, format!("bad request: {e}")).into_response();
        }
    };
    // The per-session secret the pusher minted for this session (A2): required — a session
    // restored without a recorded secret would be unreachable yet exist.
    let header_secret = match headers
        .get("x-otto-session-secret")
        .and_then(|v| v.to_str().ok())
    {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                "missing or empty X-Otto-Session-Secret header",
            )
                .into_response();
        }
    };
    match state.service.accept_promotion(&bundle).await {
        Ok(session) => {
            state.record_session_secret(session, header_secret);
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
/// can pull it back. `403` unless `--accept-promotions`. Authorized by the **session's** per-session
/// secret (recorded at `/promote`), never the machine-wide secret; a successful export disposes the
/// secret (demote consumes the credential). The bundle's workspace snapshot is gate-filtered
/// (secrets never leave here).
async fn export_handler(
    headers: HeaderMap,
    State(state): State<Arc<ServeState>>,
    body: axum::body::Bytes,
) -> Response {
    if !state.accept_promotions {
        return (StatusCode::FORBIDDEN, "promotion acceptance disabled").into_response();
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
    // The session's per-session secret authorizes its export — `None` means never-promoted-here
    // or already-disposed (indistinguishable by design, and both `401`). The machine secret is
    // admission-only and no longer authorizes an export.
    let Some(secret) = state.session_secret(session) else {
        return (StatusCode::UNAUTHORIZED, "missing or invalid bearer token").into_response();
    };
    if !authorized(&headers, Some(&secret)) {
        return (StatusCode::UNAUTHORIZED, "missing or invalid bearer token").into_response();
    }
    match state.service.export_promotion(session).await {
        Ok(bundle) => {
            // Demote consumes the credential: dispose BEFORE returning the bundle. A store row
            // that vanishes between the check above and this read keeps its fail-closed 404.
            state.dispose_session_secret(session);
            axum::Json(bundle).into_response()
        }
        // Unknown session (snapshot errors when the row is absent) → 404, not a 500.
        Err(_) => (StatusCode::NOT_FOUND, "unknown session").into_response(),
    }
}

/// The writer half of a split WebSocket — what events and frames are sent through.
type WsWriter = SplitSink<WebSocket, Message>;

/// The reader half of a split WebSocket — inbound frames are read through this.
type WsReader = SplitStream<WebSocket>;

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
/// The writer is shared through a mutex so the in-turn reader loop (`run_turn_loop`) can also
/// write (a mid-turn `Refresh` replies `LoggedIn`) while the turn's sink is alive.
struct WsSink<'a> {
    writer: &'a tokio::sync::Mutex<WsWriter>,
}

#[async_trait::async_trait]
impl EventSink for WsSink<'_> {
    async fn emit(&mut self, event: &Event) -> anyhow::Result<()> {
        let mut writer = self.writer.lock().await;
        send_msg(
            &mut writer,
            &ServerMessage::Event {
                event: event.clone(),
            },
        )
        .await
    }
}

/// The result of racing a turn future against inbound socket frames.
enum TurnLoopOutcome {
    /// The turn future resolved (successfully or with an error). The connection stays open.
    Finished(Option<anyhow::Error>),
    /// An explicit `Abort` or a disconnect ended things — the caller must stop reading frames
    /// entirely.
    StopOuterLoop,
    /// The access token failed its per-command re-verification on an in-turn command (`Users`
    /// mode; §7.2). The in-flight turn was aborted; the caller sends the opaque `AUTH_FAILED`
    /// error and must stop reading frames entirely.
    AuthFailed,
}

/// Report a turn's `TurnLoopOutcome` on the socket (an error frame, or nothing on success) and
/// say whether the caller must `break 'outer`. Shared by every command that starts a turn
/// (`SendPrompt`, `RunCommand`, `RunAgent`) so this handling can never drift between them.
async fn report_turn_outcome(outcome: TurnLoopOutcome, writer: &mut WsWriter) -> bool {
    match outcome {
        TurnLoopOutcome::Finished(Some(e)) => {
            let _ = send_msg(
                writer,
                &ServerMessage::Error {
                    message: e.to_string(),
                },
            )
            .await;
            false
        }
        TurnLoopOutcome::Finished(None) => false,
        TurnLoopOutcome::StopOuterLoop => true,
        TurnLoopOutcome::AuthFailed => {
            let _ = send_msg(
                writer,
                &ServerMessage::Error {
                    message: AUTH_FAILED.to_string(),
                },
            )
            .await;
            true
        }
    }
}

/// Assert a freshly-rotated pair's principal matches the connection's bound owner (finding 3).
/// The `Authenticator` seam's `rotate_refresh` mints for whoever presented the refresh token
/// but returns only the pair, so the minted access token is re-verified to recover the
/// principal. A mismatch — alice's connection presenting bob's refresh token — fails closed:
/// the token is already consumed, and the connection must not keep alice's session authorized
/// against a pair minted for bob.
async fn rotated_pair_owned_by(
    authenticator: &dyn Authenticator,
    pair: &TokenPair,
    owner: &UserId,
) -> bool {
    match authenticator.verify_access(&pair.access_token).await {
        Ok(principal) => principal.user == *owner,
        Err(_) => false,
    }
}

/// Re-verify the connection's access token for `Users` mode (§7.2): the token is checked on
/// every command, not only at the handshake. Non-`Users` modes have no per-command token to
/// check (`SingleUser` holds no credential, `Machine` is secret-authenticated) and always pass.
/// The principal the token resolves to must also be the connection's bound owner — a
/// connection authenticated as alice must not be able to present a DIFFERENT user's valid
/// access token and stay authorized to alice's session (finding 3, defense-in-depth).
/// Returns `false` when re-verification fails — the token is revoked/expired, resolves to a
/// different owner, or a `Users` server somehow has no authenticator (fail closed).
async fn re_verify_access_token(
    state: &ServeState,
    access_token: Option<&str>,
    owner: &UserId,
) -> bool {
    if state.auth.mode != AuthMode::Users {
        return true;
    }
    let Some(token) = access_token else {
        return false;
    };
    match state.authenticator() {
        Some(authenticator) => match authenticator.verify_access(token).await {
            Ok(principal) => principal.user == *owner,
            Err(_) => false,
        },
        None => false,
    }
}

/// Drive `turn` to completion while concurrently reading inbound frames for
/// `ApproveDiff`/`Pause`/`Resume`/`Abort`. Shared by every command that starts a turn
/// (`SendPrompt`, `RunCommand`) so their concurrency behavior can never drift apart. `owner` is
/// the connection-scoped principal (A9): the in-flight `Abort` acts for it. `writer` is the
/// socket's shared writer (the turn's own sink writes events through the same mutex), which a
/// mid-turn `Refresh` uses to reply `LoggedIn`. `access_token` is the connection's bearer
/// (`None` outside `Users`); it is re-verified on every in-turn command (§7.2), so a
/// revoked/expired token can neither approve a gated edit nor issue in-turn controls until the
/// turn ends — a failure aborts the turn and returns `AuthFailed`. Identity commands ride ahead
/// of that re-verify, matching the main loop: a mid-turn `Refresh` rotates the pair (reply
/// `LoggedIn`, rebinding `*access_token`) even when the access token is revoked/expired — the
/// §8 recovery path — while `Login`/`Attach`/`Logout` are exempted but otherwise ignored here.
// The per-command re-verification (Finding I2) pushed this past clippy's 7-argument threshold.
// Folding `owner`+`access_token` into a struct would split the connection's `ConnIdentity` from
// the resolved (Machine-adopted) session owner — a worse signature than a long one.
#[allow(clippy::too_many_arguments)]
async fn run_turn_loop(
    turn: impl std::future::Future<Output = anyhow::Result<TurnOutcome>>,
    reader: &mut WsReader,
    writer: &tokio::sync::Mutex<WsWriter>,
    approvals: &ApprovalRegistry,
    pause_state: &PauseState,
    state: &ServeState,
    session: SessionId,
    owner: &UserId,
    access_token: &mut Option<String>,
) -> TurnLoopOutcome {
    tokio::pin!(turn);
    loop {
        tokio::select! {
            res = &mut turn => {
                let err = res.err();
                approvals.clear();
                // Drop any leftover pause flag so a Pause that arrived but was never resumed
                // before the turn ended cannot pre-pause the next one.
                pause_state.resume_all();
                return TurnLoopOutcome::Finished(err);
            }
            inbound = reader.next() => match inbound {
                Some(Ok(Message::Text(t))) => {
                    let Ok(command) = serde_json::from_str::<Command>(t.as_str()) else {
                        // A malformed frame mid-turn is ignored (as before).
                        continue;
                    };
                    // §7.2: the access token is re-verified on every in-turn command, not only
                    // at the main loop's dispatch — a revoked/expired token must not approve a
                    // gated edit or issue in-turn controls while a turn is in flight. Identity
                    // commands ride ahead, as in the main loop (§8): a mid-turn `Refresh` is the
                    // recovery path for exactly the revoked/expired-token case this check guards
                    // against, and `Login`/`Attach`/`Logout` are lifecycle frames, not commands.
                    if !matches!(
                        &command,
                        Command::Login { .. }
                            | Command::Attach { .. }
                            | Command::Refresh { .. }
                            | Command::Logout
                    ) && !re_verify_access_token(state, access_token.as_deref(), owner).await
                    {
                        eprintln!("serve: per-command access-token re-verification failed mid-turn");
                        let _ = state.service.abort(owner, session).await;
                        approvals.clear();
                        pause_state.resume_all();
                        return TurnLoopOutcome::AuthFailed;
                    }
                    match command {
                        Command::Refresh { refresh_token } => {
                            // §8: service the mid-turn `Refresh` instead of dropping it. Rotate
                            // the refresh token, reply `LoggedIn`, and rebind the connection's
                            // access token (the caller's `ConnIdentity.access_token`, passed by
                            // `&mut`) so the next command — in-turn or after — re-verifies
                            // against the fresh pair. Mirrors the main loop's `Refresh` arm.
                            let Some(authenticator) = state.authenticator() else {
                                eprintln!("serve: mid-turn refresh on a non-Users connection");
                                let mut guard = writer.lock().await;
                                let _ = send_msg(
                                    &mut guard,
                                    &ServerMessage::Error {
                                        message: AUTH_FAILED.to_string(),
                                    },
                                )
                                .await;
                                drop(guard);
                                approvals.clear();
                                pause_state.resume_all();
                                return TurnLoopOutcome::StopOuterLoop;
                            };
                            match authenticator.rotate_refresh(&refresh_token).await {
                                Ok(pair) => {
                                    // The rotated pair belongs to whoever presented the
                                    // refresh token; it must be this connection's owner, or
                                    // the pair is rejected and the connection closed (finding
                                    // 3). A `LoggedIn` for a different user would keep the
                                    // socket authorized to the owner's session.
                                    if !rotated_pair_owned_by(
                                        authenticator.as_ref(),
                                        &pair,
                                        owner,
                                    )
                                    .await
                                    {
                                        eprintln!(
                                            "serve: mid-turn refresh rotated a pair for a \
                                             principal other than the connection owner"
                                        );
                                        let mut guard = writer.lock().await;
                                        let _ = send_msg(
                                            &mut guard,
                                            &ServerMessage::Error {
                                                message: AUTH_FAILED.to_string(),
                                            },
                                        )
                                        .await;
                                        drop(guard);
                                        approvals.clear();
                                        pause_state.resume_all();
                                        return TurnLoopOutcome::StopOuterLoop;
                                    }
                                    let logged_in = ServerMessage::LoggedIn {
                                        user: owner.clone(),
                                        access_token: pair.access_token.clone(),
                                        expires_at: pair.expires_at,
                                        refresh_token: pair.refresh_token,
                                    };
                                    *access_token = Some(pair.access_token);
                                    let mut guard = writer.lock().await;
                                    let ok =
                                        send_msg(&mut guard, &logged_in).await.is_ok();
                                    drop(guard);
                                    if !ok {
                                        approvals.clear();
                                        pause_state.resume_all();
                                        return TurnLoopOutcome::StopOuterLoop;
                                    }
                                }
                                Err(e) => {
                                    eprintln!("serve: mid-turn refresh rotation failed: {e:?}");
                                    let mut guard = writer.lock().await;
                                    let _ = send_msg(
                                        &mut guard,
                                        &ServerMessage::Error {
                                            message: AUTH_FAILED.to_string(),
                                        },
                                    )
                                    .await;
                                    drop(guard);
                                    approvals.clear();
                                    pause_state.resume_all();
                                    return TurnLoopOutcome::StopOuterLoop;
                                }
                            }
                        }
                        // Exempted from the re-verify above but otherwise not serviced mid-turn:
                        // a re-`Login`/re-`Attach` cannot rebind the principal here (the main
                        // loop rejects them with the opaque error + close), and `Logout`'s
                        // revocation is the main loop's job. They are ignored (the turn goes on).
                        Command::Login { .. } | Command::Attach { .. } | Command::Logout => {}
                        Command::ApproveDiff { id, approved, .. } => {
                            approvals.resolve(id, approved);
                        }
                        Command::Pause { .. } => {
                            pause_state.pause();
                        }
                        Command::Resume { .. } => {
                            pause_state.resume_all();
                        }
                        Command::Abort { .. } => {
                            let _ = state.service.abort(owner, session).await;
                            approvals.clear();
                            pause_state.resume_all();
                            return TurnLoopOutcome::StopOuterLoop;
                        }
                        // A second SendPrompt/RunCommand mid-turn is ignored (one turn at a
                        // time); other commands are no-ops here.
                        _ => {}
                    }
                }
                Some(Ok(Message::Close(_))) | None => {
                    approvals.clear();
                    pause_state.resume_all();
                    return TurnLoopOutcome::StopOuterLoop;
                }
                _ => {}
            }
        }
    }
}

/// The identity established by the handshake, threaded through the command loop so no command
/// re-constructs `UserId::local()` at its call site (A9's resolution).
struct ConnIdentity {
    owner: UserId,
    /// The access token this connection authenticated with (header, `Login`, or `Attach`);
    /// `None` in `SingleUser`/`Machine` modes, which have no per-user token to re-verify.
    access_token: Option<String>,
}

/// Resolve the connection's principal after `Hello`. On failure — a bad credential, a non-auth
/// command, a malformed frame, or a missed deadline — sends the single opaque
/// `Error { "authentication failed" }` and returns `None`; the caller closes the socket.
async fn authenticate_connection(
    reader: &mut WsReader,
    writer: &mut WsWriter,
    headers: &HeaderMap,
    params: &ConnectParams,
    state: &ServeState,
) -> Option<ConnIdentity> {
    match state.auth.mode {
        AuthMode::SingleUser => Some(ConnIdentity {
            owner: UserId::local(),
            access_token: None,
        }),
        AuthMode::Users => {
            // A valid `Authorization: Bearer` header pre-resolves a principal — skipping the
            // *deadline*, not the greeting (§7.2). A present-but-invalid header falls through to
            // the frame handshake rather than failing the connection.
            if let Some(token) = bearer_token(headers) {
                if let Some(authenticator) = state.authenticator() {
                    if let Ok(principal) = authenticator.verify_access(&token).await {
                        return Some(ConnIdentity {
                            owner: principal.user,
                            access_token: Some(token),
                        });
                    }
                }
            }
            handshake_frame(reader, writer, state, None).await
        }
        AuthMode::Machine => {
            // The credential is the per-session secret for the session named in ?session= (A5): a
            // receiver creates no sessions, so a Machine connection without a ?session= has no
            // secret to check — the single opaque failure. Header or `Attach` frame, both against
            // the session's secret; the owner is the attached session's own, adopted in
            // `resolve_session` (§6.5 / §7.2). `Machine` waits under the same handshake deadline
            // as `Users` (finding 4).
            let Some(session) = params
                .session
                .as_ref()
                .and_then(|s| uuid::Uuid::parse_str(s).ok())
            else {
                eprintln!("serve: machine attach without a session id");
                send_msg(
                    writer,
                    &ServerMessage::Error {
                        message: AUTH_FAILED.to_string(),
                    },
                )
                .await
                .ok();
                return None;
            };
            let Some(secret) = state.session_secret(SessionId(session)) else {
                eprintln!("serve: machine attach for an unknown/disposed session");
                send_msg(
                    writer,
                    &ServerMessage::Error {
                        message: AUTH_FAILED.to_string(),
                    },
                )
                .await
                .ok();
                return None;
            };
            if authorized(headers, Some(&secret)) {
                return Some(ConnIdentity {
                    owner: UserId::local(),
                    access_token: None,
                });
            }
            handshake_frame(reader, writer, state, Some(&secret)).await
        }
    }
}

/// Await and verify the first post-upgrade frame under `AuthConfig.handshake_deadline` (both
/// `Users` and `Machine` wait; `SingleUser` never reaches this — it expects no frame). It must
/// be `Login` (identity credentials → `authenticate` + `mint` → `LoggedIn`) or `Attach` (an
/// access token, or on `Machine` the session's per-session secret). `expected_machine_secret` is
/// the secret the `Machine` `Attach` arm verifies against (`None` on the `Users` path, which is
/// unchanged). Anything else is the same opaque failure. Returns the established identity.
async fn handshake_frame(
    reader: &mut WsReader,
    writer: &mut WsWriter,
    state: &ServeState,
    expected_machine_secret: Option<&str>,
) -> Option<ConnIdentity> {
    let frame = match tokio::time::timeout(state.auth.handshake_deadline, reader.next()).await {
        Err(_) => {
            eprintln!("serve: handshake deadline exceeded; closing");
            send_msg(
                writer,
                &ServerMessage::Error {
                    message: AUTH_FAILED.to_string(),
                },
            )
            .await
            .ok();
            return None;
        }
        Ok(frame) => frame,
    };
    let text = match frame {
        Some(Ok(Message::Text(t))) => t,
        // Disconnect or a non-text frame: close silently (nothing to tell the client).
        _ => return None,
    };
    let command = match serde_json::from_str::<Command>(text.as_str()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("serve: malformed handshake frame: {e}");
            send_msg(
                writer,
                &ServerMessage::Error {
                    message: AUTH_FAILED.to_string(),
                },
            )
            .await
            .ok();
            return None;
        }
    };
    match command {
        Command::Login { credentials } => {
            let Some(authenticator) = state.authenticator() else {
                // A `Users`-mode server must not exist without one; fail closed.
                send_msg(
                    writer,
                    &ServerMessage::Error {
                        message: AUTH_FAILED.to_string(),
                    },
                )
                .await
                .ok();
                return None;
            };
            let principal = match authenticator.authenticate(&credentials).await {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("serve: login failed: {e:?}");
                    send_msg(
                        writer,
                        &ServerMessage::Error {
                            message: AUTH_FAILED.to_string(),
                        },
                    )
                    .await
                    .ok();
                    return None;
                }
            };
            let pair = match authenticator.mint(&principal).await {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("serve: token mint failed: {e:?}");
                    send_msg(
                        writer,
                        &ServerMessage::Error {
                            message: AUTH_FAILED.to_string(),
                        },
                    )
                    .await
                    .ok();
                    return None;
                }
            };
            let logged_in = ServerMessage::LoggedIn {
                user: principal.user.clone(),
                access_token: pair.access_token.clone(),
                expires_at: pair.expires_at,
                refresh_token: pair.refresh_token,
            };
            if send_msg(writer, &logged_in).await.is_err() {
                return None;
            }
            Some(ConnIdentity {
                owner: principal.user,
                access_token: Some(pair.access_token),
            })
        }
        Command::Attach { token } => {
            if state.auth.mode == AuthMode::Machine {
                // The one credential Machine accepts in a frame: the attached session's
                // per-session secret, constant-time.
                if secret_matches(expected_machine_secret, &token) {
                    Some(ConnIdentity {
                        owner: UserId::local(),
                        access_token: None,
                    })
                } else {
                    eprintln!("serve: machine attach rejected");
                    send_msg(
                        writer,
                        &ServerMessage::Error {
                            message: AUTH_FAILED.to_string(),
                        },
                    )
                    .await
                    .ok();
                    None
                }
            } else {
                let Some(authenticator) = state.authenticator() else {
                    send_msg(
                        writer,
                        &ServerMessage::Error {
                            message: AUTH_FAILED.to_string(),
                        },
                    )
                    .await
                    .ok();
                    return None;
                };
                match authenticator.verify_access(&token).await {
                    Ok(principal) => Some(ConnIdentity {
                        owner: principal.user,
                        access_token: Some(token),
                    }),
                    Err(e) => {
                        eprintln!("serve: attach failed: {e:?}");
                        send_msg(
                            writer,
                            &ServerMessage::Error {
                                message: AUTH_FAILED.to_string(),
                            },
                        )
                        .await
                        .ok();
                        None
                    }
                }
            }
        }
        // Any non-auth command before authentication — including `CreateSession` — is the same
        // failure (§7.2 step 3).
        _ => {
            eprintln!("serve: non-auth command before authentication");
            send_msg(
                writer,
                &ServerMessage::Error {
                    message: AUTH_FAILED.to_string(),
                },
            )
            .await
            .ok();
            None
        }
    }
}

async fn handle_socket(
    socket: WebSocket,
    headers: HeaderMap,
    params: ConnectParams,
    state: Arc<ServeState>,
) {
    // Split up-front so the turn (writer) and inbound approvals (reader) can run concurrently.
    let (mut writer, mut reader) = socket.split();

    // `Hello` is always the first frame, in every mode — even when the upgrade header already
    // pre-resolved a principal (that skips the *deadline*, not the greeting, §7.2).
    if send_msg(
        &mut writer,
        &ServerMessage::Hello {
            auth_mode: state.auth.mode.clone(),
        },
    )
    .await
    .is_err()
    {
        return;
    }

    // Establish the connection's identity (or close on the opaque auth failure).
    let mut identity =
        match authenticate_connection(&mut reader, &mut writer, &headers, &params, &state).await {
            Some(i) => i,
            None => return,
        };

    let (session, owner) = match resolve_session(&params, &state, &identity.owner).await {
        Ok((s, o)) => (s, o),
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
            .replay_since(&owner, session, Some(after))
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
        // A `Users` connection re-verifies its access token on every non-auth command: a
        // long-lived socket must not outlive the token's expiry or its denylisting (§7.2).
        // `Refresh` rides ahead (it needs no access token) and `Logout` revokes the one we hold.
        if !matches!(
            &command,
            Command::Login { .. }
                | Command::Attach { .. }
                | Command::Refresh { .. }
                | Command::Logout
        ) && !re_verify_access_token(&state, identity.access_token.as_deref(), &identity.owner)
            .await
        {
            eprintln!("serve: per-command access-token re-verification failed");
            let _ = send_msg(
                &mut writer,
                &ServerMessage::Error {
                    message: AUTH_FAILED.to_string(),
                },
            )
            .await;
            // The connection stays open so the client can `Refresh` (§8); the command is not
            // dispatched.
            continue;
        }
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
                // Drive the turn while concurrently reading inbound approvals. The turn's sink
                // and `run_turn_loop` share `writer` through a mutex — the turn writes events
                // through the sink, and the reader loop replies to a mid-turn `Refresh` with
                // `LoggedIn` — while `run_turn_loop` borrows `reader` to poll both. The token
                // slot is passed `&mut` so a mid-turn `Refresh` can rebind it in place.
                let shared_writer = tokio::sync::Mutex::new(writer);
                let outcome = {
                    let mut sink = WsSink {
                        writer: &shared_writer,
                    };
                    let turn = state
                        .service
                        .run_prompt_with_controls(&owner, session, &text, &mut sink, controls);
                    run_turn_loop(
                        turn,
                        &mut reader,
                        &shared_writer,
                        &approvals,
                        &pause_state,
                        &state,
                        session,
                        &owner,
                        &mut identity.access_token,
                    )
                    .await
                }; // `sink` dropped here → the mutex is unguarded again

                writer = shared_writer.into_inner();
                if report_turn_outcome(outcome, &mut writer).await {
                    break 'outer;
                }
            }
            Command::Abort { .. } => {
                let _ = state.service.abort(&owner, session).await;
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
            // Handover is the one client-facing command that does not route through an
            // `EngineService` method — `handle_handover` reaches `otto_remote::promote` via the
            // `store()` accessor — so it authorizes explicitly here, for the connection's owner.
            // Promote ships a session's whole event log off-machine and demote overwrites the
            // local row including its owner; the check must never be silently droppable.
            Command::PromoteToRemote { .. } | Command::DemoteToLocal { .. } => {
                let to_remote = matches!(command, Command::PromoteToRemote { .. });
                if let Err(e) = state.service.authorize_session(&owner, session).await {
                    let _ = send_msg(
                        &mut writer,
                        &ServerMessage::Error {
                            message: e.to_string(),
                        },
                    )
                    .await;
                    continue;
                }
                handle_handover(&state, &mut writer, session, to_remote).await;
            }
            Command::RunCommand { name, args, .. } => {
                let approver = Arc::new(InteractiveApprover::new(approvals.clone()));
                let pauser = Arc::new(InteractivePauser(Arc::clone(&pause_state)));
                let shared_writer = tokio::sync::Mutex::new(writer);
                let outcome = {
                    let mut sink = WsSink {
                        writer: &shared_writer,
                    };
                    let turn = state.service.run_command_with_controls(
                        &owner, session, &name, &args, &mut sink, approver, pauser,
                    );
                    run_turn_loop(
                        turn,
                        &mut reader,
                        &shared_writer,
                        &approvals,
                        &pause_state,
                        &state,
                        session,
                        &owner,
                        &mut identity.access_token,
                    )
                    .await
                }; // `sink` dropped here → the mutex is unguarded again

                writer = shared_writer.into_inner();
                if report_turn_outcome(outcome, &mut writer).await {
                    break 'outer;
                }
            }
            Command::RunAgent { name, prompt, .. } => {
                // No `run_turn_loop`: a single TaskTool dispatch has no fs.write gate check to
                // approve and no multi-step turn to pause between steps of (see the design spec).
                let shared_writer = tokio::sync::Mutex::new(writer);
                let outcome = {
                    let mut sink = WsSink {
                        writer: &shared_writer,
                    };
                    state
                        .service
                        .run_agent_with_controls(&owner, session, &name, &prompt, &mut sink)
                        .await
                }; // `sink` dropped here → the mutex is unguarded again
                writer = shared_writer.into_inner();

                if report_turn_outcome(TurnLoopOutcome::Finished(outcome.err()), &mut writer).await
                {
                    break 'outer;
                }
            }
            // Identity commands are only valid during the handshake on a `Users` connection; a
            // re-`Login`/re-`Attach` mid-connection must not rebind the principal (and the modes
            // where they are not-applicable were announced by `Hello`). Opaque failure + close.
            Command::Login { .. } | Command::Attach { .. } => {
                let _ = send_msg(
                    &mut writer,
                    &ServerMessage::Error {
                        message: AUTH_FAILED.to_string(),
                    },
                )
                .await;
                break;
            }
            Command::Refresh { refresh_token } => {
                // Not-applicable outside `Users` (the mode was announced by `Hello`); inside it,
                // rotate the refresh token and reply with a fresh `LoggedIn`, re-binding the
                // connection's access token so the next command re-verifies against it.
                let Some(authenticator) = state.authenticator() else {
                    let _ = send_msg(
                        &mut writer,
                        &ServerMessage::Error {
                            message: AUTH_FAILED.to_string(),
                        },
                    )
                    .await;
                    break;
                };
                match authenticator.rotate_refresh(&refresh_token).await {
                    Ok(pair) => {
                        // The rotated pair belongs to whoever presented the refresh token;
                        // it must be this connection's owner, or the connection closes with
                        // the opaque error (finding 3) — a different user's refresh token
                        // must not keep the socket authorized to the owner's session.
                        if !rotated_pair_owned_by(authenticator.as_ref(), &pair, &identity.owner)
                            .await
                        {
                            eprintln!(
                                "serve: refresh rotated a pair for a principal other than the \
                                 connection owner"
                            );
                            let _ = send_msg(
                                &mut writer,
                                &ServerMessage::Error {
                                    message: AUTH_FAILED.to_string(),
                                },
                            )
                            .await;
                            break;
                        }
                        let logged_in = ServerMessage::LoggedIn {
                            user: identity.owner.clone(),
                            access_token: pair.access_token.clone(),
                            expires_at: pair.expires_at,
                            refresh_token: pair.refresh_token,
                        };
                        identity.access_token = Some(pair.access_token);
                        if send_msg(&mut writer, &logged_in).await.is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        eprintln!("serve: refresh rotation failed: {e:?}");
                        let _ = send_msg(
                            &mut writer,
                            &ServerMessage::Error {
                                message: AUTH_FAILED.to_string(),
                            },
                        )
                        .await;
                        break;
                    }
                }
            }
            Command::Logout => {
                // Not-applicable outside `Users`. Inside it: denylist the connection's access
                // token's `jti` and revoke its refresh token, abort the principal's in-flight
                // turn, send `LoggedOut`, and close — the connection does not continue
                // unauthenticated (§7.2).
                if let (Some(authenticator), Some(token)) =
                    (state.authenticator(), &identity.access_token)
                {
                    if let Err(e) = authenticator.logout(token).await {
                        eprintln!("serve: logout revocation failed: {e:?}");
                    }
                }
                let _ = state.service.abort(&owner, session).await;
                approvals.clear();
                pause_state.resume_all();
                let _ = send_msg(&mut writer, &ServerMessage::LoggedOut).await;
                break;
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
                    "remote provisioning unavailable (start otto serve with --promote-loopback, --promote-vps, --promote-microvm, or --promote-fly)"
                        .to_string(),
            },
        )
        .await;
        return;
    };

    // Vps demote: pull the session's current bundle back off the receiver and restore it into THIS
    // (source) engine, overwriting our own stale copy. Symmetric inverse of the promote push. The
    // client reconnects to us (the session is local again), so we report our own public ws base.
    // The pull is authorized by the session's per-session secret, which lives in the stored handle
    // (spec §1.3): the machine-wide pusher token is not a session credential and cannot export.
    if !to_remote {
        if let otto_remote::PromoteMode::Vps { .. } = &cfg.mode {
            // Source the live endpoint+secret from the handle a prior promote stored under
            // (session, true). No handle ⟹ nothing to pull from. Take the lock only to clone the
            // endpoint/secret, releasing it at the `;` before any await.
            let live = state
                .remotes
                .lock()
                .unwrap()
                .get(&(session, true))
                .map(|h| (h.endpoint.clone(), h.token.clone()));
            let Some((endpoint, secret)) = live else {
                let _ = send_msg(
                    writer,
                    &ServerMessage::Error {
                        message: "no active vps handover for this session; promote first"
                            .to_string(),
                    },
                )
                .await;
                return;
            };

            // Pull the current bundle off the receiver with the stored session secret. On failure,
            // leave the remote session in place (a transient pull error must not lose it).
            let bundle = match otto_remote::export_bundle(&endpoint, &secret, session).await {
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
            if let Err(e) = state.service.accept_demotion(session, &bundle).await {
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
            if let Err(e) = state.service.accept_demotion(session, &bundle).await {
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
        if let otto_remote::PromoteMode::Fly { config } = &cfg.mode {
            // Source the live app endpoint+token from the handle a prior promote stored under
            // (session, true). Clone out under the lock, release before awaiting.
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
                        message: "no active fly handover for this session; promote first"
                            .to_string(),
                    },
                )
                .await;
                return;
            };

            // Pull the current bundle off the Fly machine. On failure, leave it running and report.
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
            if let Err(e) = state.service.accept_demotion(session, &bundle).await {
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

            // Success: destroy the Fly app (we own it), then drop the handle and tell the client to
            // reconnect to us. teardown deletes the app parsed from the endpoint.
            let target = otto_remote::FlyTarget::new(config.clone());
            if let Err(e) = target
                .teardown(otto_remote::RemoteHandle::new(endpoint, token))
                .await
            {
                // Restore already committed; a failed delete only risks an orphan (idle-suspended,
                // auto_destroy-reaped). Report it but the session is local again.
                let _ = send_msg(
                    writer,
                    &ServerMessage::Error {
                        message: format!("session demoted but fly app cleanup failed: {e}"),
                    },
                )
                .await;
                state.remotes.lock().unwrap().remove(&(session, true));
                return;
            }
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
        .map(|h| (h.endpoint.clone(), h.token.clone()));
    let (endpoint, tok) = match existing {
        Some((endpoint, tok)) => (endpoint, tok),
        None => {
            let target: Box<dyn otto_remote::RemoteTarget> = match &cfg.mode {
                otto_remote::PromoteMode::Loopback { base_dir } => Box::new(LoopbackTarget::new(
                    AuthConfig {
                        mode: AuthMode::SingleUser,
                        authenticator: None,
                        promotion_secret: None,
                        handshake_deadline: std::time::Duration::from_secs(10),
                    },
                    base_dir.clone(),
                    to_remote,
                )),
                otto_remote::PromoteMode::Vps { endpoint } => Box::new(
                    otto_remote::VpsTarget::new(endpoint.clone(), cfg.token.clone()),
                ),
                otto_remote::PromoteMode::Microvm { config } => {
                    #[cfg(feature = "firecracker")]
                    let provisioner: std::sync::Arc<
                        dyn otto_remote::Provisioner,
                    > = std::sync::Arc::new(otto_remote::FirecrackerProvisioner::new(
                        config.clone(),
                        crate::mint_session_secret(),
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
                otto_remote::PromoteMode::Fly { config } => {
                    Box::new(otto_remote::FlyTarget::new(config.clone()))
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
            let tok = handle.token.clone();
            // Retain BEFORE replying: for loopback, dropping the handle aborts the provisioned
            // engine; for vps the handle's shutdown is None, so retention is cheap and harmless.
            state
                .remotes
                .lock()
                .unwrap()
                .insert((session, to_remote), handle);
            (endpoint, tok)
        }
    };
    let msg = if to_remote {
        ServerMessage::Promoted {
            session,
            endpoint,
            token: tok,
        }
    } else {
        ServerMessage::Demoted { session, endpoint }
    };
    let _ = send_msg(writer, &msg).await;
}

/// Resolve the session for this connection and return `(session, owner)` — the connection-scoped
/// principal threaded through the whole loop (A9's resolution). The explicit `?session=` arm is
/// ownership-checked: attaching to a session the principal does not own fails byte-for-byte
/// identically to a nonexistent id, so the API is not an existence oracle. On a `Machine`
/// receiver the promotion secret is authority over every session promoted onto it, so the check
/// is existence and the connection adopts the session's own owner (§6.5). The `None` arm creates
/// a session owned by the principal — rejected on `Machine`, which hosts only promoted sessions.
async fn resolve_session(
    params: &ConnectParams,
    state: &ServeState,
    owner: &UserId,
) -> anyhow::Result<(SessionId, UserId)> {
    match &params.session {
        Some(s) => {
            let uuid = uuid::Uuid::parse_str(s)?;
            let session = SessionId(uuid);
            let actual = match state.auth.mode {
                AuthMode::Machine => {
                    // Existence only — the machine credential already authenticated the holder;
                    // the attached session's owner is adopted, whatever it is.
                    state
                        .service
                        .store()
                        .owner_of(session)
                        .await
                        .map_err(|_| anyhow::anyhow!("no session {}", session.0))?
                }
                _ => {
                    // Same shared message for "not yours" and "not there".
                    state.service.authorize_session(owner, session).await?;
                    owner.clone()
                }
            };
            Ok((session, actual))
        }
        None => {
            // A Machine receiver creates no sessions; an `Attach` without `?session=` has no owner
            // to adopt (§7.2 step 4).
            if state.auth.mode == AuthMode::Machine {
                anyhow::bail!("machine receivers do not create sessions; pass ?session=");
            }
            let session = state
                .service
                .create_session(owner, "(serve/ws)", &serde_json::json!({}))
                .await?;
            Ok((session, owner.clone()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use otto_engine_core::Router;
    use otto_engine_core::traits::Workspace;
    use otto_persistence::SessionStore;
    use otto_providers::LocalProvider;
    use otto_router::SingleProviderRouter;
    use otto_workspace::LocalWorkspace;
    use std::path::PathBuf;
    use std::sync::Arc;

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

    /// An `Authorization: Bearer` header carrying `token`.
    fn bearer_headers(token: &str) -> axum::http::HeaderMap {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
        );
        headers
    }

    /// A `ServeState` in `Machine` mode over a real (offline, deterministic) `EngineService`.
    /// The test module shares `serve.rs`, so the private fields are constructible directly — the
    /// fixture is the smallest honest way to exercise `machine_workspace_authorized`.
    async fn machine_state_fixture(
        dir: &tempfile::TempDir,
        promotion_secret: Option<&str>,
    ) -> ServeState {
        let router: Arc<dyn Router> =
            Arc::new(SingleProviderRouter::new(Arc::new(LocalProvider::new())));
        let workspace: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(dir.path()));
        let tools_ws: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(dir.path()));
        let tools = Arc::new(crate::build_tool_registry(
            tools_ws,
            dir.path().to_path_buf(),
        ));
        let store: Arc<dyn SessionStore> = Arc::new(
            otto_persistence::SqliteStore::open(dir.path().join("s.db"))
                .await
                .unwrap(),
        );
        let service = EngineService::new(
            store,
            Arc::new(crate::build_default_registry()),
            router,
            workspace,
            tools,
        );
        ServeState {
            service,
            auth: AuthConfig {
                mode: AuthMode::Machine,
                authenticator: None,
                promotion_secret: promotion_secret.map(String::from),
                handshake_deadline: std::time::Duration::from_secs(10),
            },
            capabilities: crate::build_capabilities(),
            promote: None,
            accept_promotions: true,
            public_ws_base: None,
            remotes: Mutex::new(HashMap::new()),
            session_secrets: Mutex::new(HashMap::new()),
        }
    }

    #[tokio::test]
    async fn machine_workspace_authorized_accepts_machine_and_session_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let state = machine_state_fixture(&dir, Some("machine-secret")).await;
        let session = SessionId(uuid::Uuid::new_v4());
        state.record_session_secret(session, "session-secret".to_string());

        // A6: the machine secret (operator path) and any live session secret both authorize.
        assert!(state.machine_workspace_authorized(&bearer_headers("machine-secret")));
        assert!(state.machine_workspace_authorized(&bearer_headers("session-secret")));
        // A wrong secret — and no header at all — are rejected.
        assert!(!state.machine_workspace_authorized(&bearer_headers("wrong-secret")));
        assert!(!state.machine_workspace_authorized(&HeaderMap::new()));
        // Demote consumes the credential: the disposed secret no longer authorizes /workspace.
        state.dispose_session_secret(session);
        assert!(!state.machine_workspace_authorized(&bearer_headers("session-secret")));
    }

    #[tokio::test]
    async fn machine_workspace_authorized_without_machine_secret_uses_session_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let state = machine_state_fixture(&dir, None).await;
        let session = SessionId(uuid::Uuid::new_v4());
        state.record_session_secret(session, "session-secret".to_string());

        assert!(state.machine_workspace_authorized(&bearer_headers("session-secret")));
        assert!(!state.machine_workspace_authorized(&bearer_headers("wrong-secret")));
    }

    #[test]
    fn pre_ownership_bundle_detected_when_session_lacks_owner() {
        // The pre-ownership shape: a bundle whose `session` object carries no `owner` key.
        let legacy = br#"{"session":{"id":"00000000-0000-0000-0000-000000000000","goal":"g","status":"active","config":{},"events":[],"turns":[]},"workspace":{}}"#;
        assert!(is_pre_ownership_bundle(legacy));
    }

    #[test]
    fn pre_ownership_bundle_false_for_a_valid_bundle() {
        // A current bundle's `session` carries `owner` — not a legacy shape.
        let valid = br#"{"session":{"id":"00000000-0000-0000-0000-000000000000","owner":"local","goal":"g","status":"active","config":{},"events":[],"turns":[]},"workspace":{}}"#;
        assert!(!is_pre_ownership_bundle(valid));
    }

    #[test]
    fn pre_ownership_bundle_false_for_garbage_and_non_bundle_bodies() {
        assert!(!is_pre_ownership_bundle(b"not json"));
        assert!(!is_pre_ownership_bundle(b"{}"));
        assert!(!is_pre_ownership_bundle(br#"{"other": 1}"#));
        // A `session` that is present but not an object keeps the ordinary bad-request path.
        assert!(!is_pre_ownership_bundle(br#"{"session": 42}"#));
    }
}
