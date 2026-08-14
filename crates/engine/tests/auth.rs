//! End-to-end auth: the three serve modes, the `Hello`/`Login`/`Attach` handshake, and the
//! per-mode HTTP route credentials (spec §6.5 / §7.2 / §7.3; success criteria 2–4). Runs against
//! `FakeAuthenticator` (no database, no network) on loopback ephemeral ports; the timeout tests
//! use the injectable `handshake_deadline` so they do not block the 10-second default each.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use otto_auth::testing::FakeAuthenticator;
use otto_engine::{
    EngineService, build_default_registry, build_tool_registry, build_tool_registry_approving,
    serve_app, serve_run,
};
use otto_engine_core::auth::{AuthConfig, AuthError, Authenticator, Principal, TokenPair};
use otto_engine_core::traits::Workspace;
use otto_providers::ScriptedProvider;
use otto_router::SingleProviderRouter;
use otto_workspace::LocalWorkspace;
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

/// The promotion secret the `Machine` harnesses serve.
const SECRET: &str = "test-promotion-secret";
/// Short enough that the timeout tests are fast, long enough that a legitimate handshake never
/// races it.
const HANDSHAKE_DEADLINE: Duration = Duration::from_millis(500);

fn alice() -> otto_protocol::UserId {
    otto_protocol::UserId::parse("alice").unwrap()
}

fn bob() -> otto_protocol::UserId {
    otto_protocol::UserId::parse("bob").unwrap()
}

/// A fixed manifest the test servers report, so the assertions below are deterministic and also
/// prove non-default values are threaded through (not hardcoded false).
fn test_capabilities() -> otto_protocol::CapabilitiesManifest {
    otto_protocol::CapabilitiesManifest {
        engine_remote: false,
        local_llm: true,
        remote_llm: false,
        sandbox: true,
    }
}

/// An `EngineService` over a fresh tempdir store + workspace. The store lives at the conventional
/// `s.db` path inside `dir`, which the store-inspection helpers re-open.
async fn build_service(dir: &tempfile::TempDir) -> EngineService {
    build_service_inner(dir, false).await
}

/// An `EngineService` whose `fs.write` is gated `Ask` (interactive approval mode), so a turn
/// whose Coder proposes edits parks on the approver until an `ApproveDiff` (or disconnect).
async fn build_service_approving(dir: &tempfile::TempDir) -> EngineService {
    build_service_inner(dir, true).await
}

async fn build_service_inner(dir: &tempfile::TempDir, approving: bool) -> EngineService {
    let provider = ScriptedProvider::new("{}")
        .on(
            "edits",
            r#"{"edits": [{"path": "out.txt", "contents": "hi add a greeting"}]}"#,
        )
        .on("milestones", r#"{"milestones": [{"description": "x"}]}"#);
    let router: Arc<dyn otto_engine_core::Router> =
        Arc::new(SingleProviderRouter::new(Arc::new(provider)));
    let workspace: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(dir.path()));
    let tools_ws: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(dir.path()));
    let tools = if approving {
        Arc::new(build_tool_registry_approving(
            tools_ws,
            dir.path().to_path_buf(),
        ))
    } else {
        Arc::new(build_tool_registry(tools_ws, dir.path().to_path_buf()))
    };
    let store: Arc<dyn otto_persistence::SessionStore> = Arc::new(
        otto_persistence::SqliteStore::open(dir.path().join("s.db"))
            .await
            .unwrap(),
    );
    EngineService::new(
        store,
        Arc::new(build_default_registry()),
        router,
        workspace,
        tools,
    )
}

fn store_path(dir: &tempfile::TempDir) -> PathBuf {
    dir.path().join("s.db")
}

/// The number of sessions the server's store currently holds — the "no session was created"
/// assertion behind success criterion 2. The server keeps its store open; a second connection
/// reads the same sqlite file.
async fn session_count(dir: &tempfile::TempDir) -> i64 {
    let pool =
        sqlx::sqlite::SqlitePool::connect(&format!("sqlite://{}", store_path(dir).display()))
            .await
            .unwrap();
    let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM sessions")
        .fetch_one(&pool)
        .await
        .unwrap();
    count
}

async fn serve(app: axum::Router) -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        serve_run(listener, app, None).await.unwrap();
    });
    port
}

/// A `Users`-mode server with the fixed `FakeAuthenticator` (authenticates every login as
/// `alice`) and a short handshake deadline. The fake handle is returned so tests can mint
/// cross-tenant tokens directly through it.
struct UsersServer {
    port: u16,
    dir: tempfile::TempDir,
    fake: Arc<FakeAuthenticator>,
}

async fn start_users() -> UsersServer {
    let fake = Arc::new(FakeAuthenticator::new(alice()));
    let (port, dir) =
        start_users_with(Arc::clone(&fake) as Arc<dyn otto_engine_core::Authenticator>).await;
    UsersServer { port, dir, fake }
}

/// A `Users`-mode server over an arbitrary authenticator (the rejecting double for the
/// bad-credential test).
async fn start_users_with(
    authenticator: Arc<dyn otto_engine_core::Authenticator>,
) -> (u16, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let service = build_service(&dir).await;
    let auth = AuthConfig {
        mode: otto_protocol::AuthMode::Users,
        authenticator: Some(authenticator),
        promotion_secret: None,
        handshake_deadline: HANDSHAKE_DEADLINE,
    };
    let app = serve_app(service, auth, test_capabilities(), None, false);
    let port = serve(app).await;
    (port, dir)
}

/// A `Users`-mode server whose tool gate asks for approval on `fs.write` (the interactive
/// approver parks the turn), so a test can drive in-turn commands while a turn is in flight.
async fn start_users_approving() -> UsersServer {
    let dir = tempfile::tempdir().unwrap();
    let service = build_service_approving(&dir).await;
    let fake = Arc::new(FakeAuthenticator::new(alice()));
    let auth = AuthConfig {
        mode: otto_protocol::AuthMode::Users,
        authenticator: Some(Arc::clone(&fake) as Arc<dyn otto_engine_core::Authenticator>),
        promotion_secret: None,
        handshake_deadline: HANDSHAKE_DEADLINE,
    };
    let app = serve_app(service, auth, test_capabilities(), None, false);
    let port = serve(app).await;
    UsersServer { port, dir, fake }
}

async fn start_single_user() -> (u16, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let service = build_service(&dir).await;
    let auth = AuthConfig {
        mode: otto_protocol::AuthMode::SingleUser,
        authenticator: None,
        promotion_secret: None,
        handshake_deadline: HANDSHAKE_DEADLINE,
    };
    let app = serve_app(service, auth, test_capabilities(), None, false);
    let port = serve(app).await;
    (port, dir)
}

async fn start_machine() -> (u16, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let service = build_service(&dir).await;
    let auth = AuthConfig {
        mode: otto_protocol::AuthMode::Machine,
        authenticator: None,
        promotion_secret: Some(SECRET.to_string()),
        handshake_deadline: HANDSHAKE_DEADLINE,
    };
    let app = serve_app(service, auth, test_capabilities(), None, false);
    let port = serve(app).await;
    (port, dir)
}

/// An authenticator that rejects every credential — the seam-side failure path the
/// `FakeAuthenticator` cannot produce (it accepts anything as its fixed principal).
struct RejectingAuthenticator;

#[async_trait]
impl otto_engine_core::Authenticator for RejectingAuthenticator {
    async fn authenticate(&self, _c: &otto_protocol::Credentials) -> Result<Principal, AuthError> {
        Err(AuthError::InvalidCredentials)
    }
    async fn mint(&self, _p: &Principal) -> Result<TokenPair, AuthError> {
        Err(AuthError::InvalidCredentials)
    }
    async fn verify_access(&self, _t: &str) -> Result<Principal, AuthError> {
        Err(AuthError::InvalidCredentials)
    }
    async fn rotate_refresh(&self, _r: &str) -> Result<TokenPair, AuthError> {
        Err(AuthError::InvalidCredentials)
    }
    async fn logout(&self, _a: &str) -> Result<(), AuthError> {
        Ok(())
    }
}

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

fn request(port: u16, query: &str) -> tokio_tungstenite::tungstenite::handshake::client::Request {
    let url = format!("ws://127.0.0.1:{port}/ws{query}");
    url.into_client_request().unwrap()
}

async fn connect(req: tokio_tungstenite::tungstenite::handshake::client::Request) -> Ws {
    let (ws, _) = tokio_tungstenite::connect_async(req)
        .await
        .expect("connect");
    ws
}

async fn send_cmd(ws: &mut Ws, cmd: &otto_protocol::Command) {
    ws.send(Message::Text(serde_json::to_string(cmd).unwrap()))
        .await
        .unwrap();
}

/// Receive the next text frame as JSON (panics on close/non-text or stream end).
async fn next_json(ws: &mut Ws) -> Value {
    next_json_opt(ws).await.expect("expected a frame")
}

/// Receive the next text frame as JSON, or None if the stream ended / a non-text frame arrived.
async fn next_json_opt(ws: &mut Ws) -> Option<Value> {
    loop {
        match ws.next().await {
            Some(Ok(Message::Text(t))) => return Some(serde_json::from_str(t.as_str()).unwrap()),
            Some(Ok(Message::Close(_))) | None => return None,
            Some(Ok(_)) => continue, // skip ping/pong/binary
            Some(Err(_)) => return None,
        }
    }
}

/// Assert the first frame is `Hello`, returning it.
async fn hello(ws: &mut Ws) -> Value {
    let hello = next_json(ws).await;
    assert_eq!(hello["type"], "hello");
    hello
}

/// `Login` this connection as the fake's principal, returning the `LoggedIn` frame. Caller must
/// have consumed `Hello` first.
async fn login(ws: &mut Ws) -> Value {
    let cmd = otto_protocol::Command::Login {
        credentials: otto_protocol::Credentials::Totp {
            user: alice(),
            code: "000000".into(),
        },
    };
    send_cmd(ws, &cmd).await;
    let logged_in = next_json(ws).await;
    assert_eq!(logged_in["type"], "logged_in");
    logged_in
}

/// Success criterion 2 (no-auth-frame half): a connection that sends nothing within the
/// handshake deadline gets the opaque Error, is closed, and creates no session.
#[tokio::test]
async fn no_auth_frame_times_out() {
    let server = start_users().await;
    let mut ws = connect(request(server.port, "")).await;
    let hello = hello(&mut ws).await;
    assert_eq!(hello["auth_mode"], "users");

    // Send nothing: the deadline expires, an opaque Error arrives, and the server closes.
    let frame = next_json(&mut ws).await;
    assert_eq!(frame["type"], "error");
    assert_eq!(frame["message"], "authentication failed");
    assert!(
        next_json_opt(&mut ws).await.is_none(),
        "connection must close after the failed handshake"
    );

    assert_eq!(
        session_count(&server.dir).await,
        0,
        "a timed-out handshake must create no session"
    );
}

/// A11: a bad credential produces the one opaque message, never a cause-specific one, and the
/// connection is closed without creating a session.
#[tokio::test]
async fn wrong_code_fails_with_the_opaque_message() {
    let (port, dir) = start_users_with(Arc::new(RejectingAuthenticator)).await;
    let mut ws = connect(request(port, "")).await;
    let hello = hello(&mut ws).await;
    assert_eq!(hello["auth_mode"], "users");

    let cmd = otto_protocol::Command::Login {
        credentials: otto_protocol::Credentials::Totp {
            user: alice(),
            code: "000000".into(),
        },
    };
    send_cmd(&mut ws, &cmd).await;

    let frame = next_json(&mut ws).await;
    assert_eq!(frame["type"], "error");
    assert_eq!(frame["message"], "authentication failed");
    assert!(
        next_json_opt(&mut ws).await.is_none(),
        "connection must close after a failed login"
    );
    assert_eq!(
        session_count(&dir).await,
        0,
        "a failed login must create no session"
    );
}

/// `Login` → `LoggedIn` (minted pair) then `Ready`.
#[tokio::test]
async fn login_reaches_ready_and_mints() {
    let server = start_users().await;
    let mut ws = connect(request(server.port, "")).await;
    let hello = hello(&mut ws).await;
    assert_eq!(hello["auth_mode"], "users");

    let logged_in = login(&mut ws).await;
    assert_eq!(logged_in["user"], "alice");
    assert!(!logged_in["access_token"].as_str().unwrap().is_empty());
    assert!(!logged_in["refresh_token"].as_str().unwrap().is_empty());

    let ready = next_json(&mut ws).await;
    assert_eq!(ready["type"], "ready");
    assert!(
        ready["session"]
            .as_str()
            .unwrap()
            .parse::<uuid::Uuid>()
            .is_ok()
    );
}

/// `Attach` with a token minted by a prior `Login` reaches `Ready` on a fresh connection.
#[tokio::test]
async fn attach_with_a_minted_token() {
    let server = start_users().await;
    let mut ws1 = connect(request(server.port, "")).await;
    hello(&mut ws1).await;
    let logged_in = login(&mut ws1).await;
    let token = logged_in["access_token"].as_str().unwrap().to_string();
    drop(ws1);

    let mut ws2 = connect(request(server.port, "")).await;
    hello(&mut ws2).await;
    send_cmd(&mut ws2, &otto_protocol::Command::Attach { token }).await;
    // Attach sends no LoggedIn; the next frame is Ready.
    let ready = next_json(&mut ws2).await;
    assert_eq!(ready["type"], "ready");
}

/// Success criterion 3: attaching to a session owned by another principal fails byte-for-byte
/// identically to attaching to a nonexistent id — the API is not an existence oracle.
#[tokio::test]
async fn cross_tenant_attach_is_indistinguishable_from_nonexistent() {
    let server = start_users().await;

    // Alice's session, seeded directly so bob has something real to aim at.
    let store = otto_persistence::SqliteStore::open(store_path(&server.dir))
        .await
        .unwrap();
    let alice_session =
        otto_persistence::SessionStore::create_session(&store, &alice(), "alice's", &json!({}))
            .await
            .unwrap();

    // A token minted for bob (the fake can mint for any principal).
    let bob_token = server
        .fake
        .mint(&Principal { user: bob() })
        .await
        .unwrap()
        .access_token;

    // Attach as bob to alice's real session.
    let mut ws1 = connect(request(
        server.port,
        &format!("?session={}", alice_session.0),
    ))
    .await;
    hello(&mut ws1).await;
    send_cmd(
        &mut ws1,
        &otto_protocol::Command::Attach {
            token: bob_token.clone(),
        },
    )
    .await;
    let err1 = next_json(&mut ws1).await;
    assert_eq!(err1["type"], "error");

    // Attach as bob to a random (nonexistent) session.
    let random = otto_protocol::SessionId::new().0.to_string();
    let mut ws2 = connect(request(server.port, &format!("?session={random}"))).await;
    hello(&mut ws2).await;
    send_cmd(
        &mut ws2,
        &otto_protocol::Command::Attach { token: bob_token },
    )
    .await;
    let err2 = next_json(&mut ws2).await;
    assert_eq!(err2["type"], "error");

    // Both are the shared `no session <id>` template (the only difference is the id the caller
    // itself supplied), so a wrong-owner attach is indistinguishable from a nonexistent one —
    // byte-for-byte once the ids are aligned (success criterion 3).
    let msg1 = err1["message"].as_str().unwrap();
    let msg2 = err2["message"].as_str().unwrap();
    assert_eq!(
        msg1.strip_prefix("no session "),
        Some(alice_session.0.to_string().as_str()),
        "wrong-owner attach must use the standard not-found template: {err1}"
    );
    assert_eq!(
        msg2.strip_prefix("no session "),
        Some(random.as_str()),
        "nonexistent attach must use the standard not-found template: {err2}"
    );
    // The residue past the id is identical — no ownership signal exists in either message.
    assert_eq!(
        &msg1[.."no session ".len()],
        &msg2[.."no session ".len()],
        "the two messages must differ only in the client-supplied id"
    );
}

/// `?token=` is deleted: a query-string bearer is inert, and a connection presenting only it hits
/// the ordinary handshake deadline.
#[tokio::test]
async fn query_token_no_longer_authenticates() {
    let server = start_users().await;
    let mut ws = connect(request(server.port, "?token=anything")).await;
    let hello = hello(&mut ws).await;
    assert_eq!(hello["auth_mode"], "users");

    let frame = next_json(&mut ws).await;
    assert_eq!(frame["type"], "error");
    assert_eq!(frame["message"], "authentication failed");
    assert!(
        next_json_opt(&mut ws).await.is_none(),
        "connection must close after the failed handshake"
    );
}

/// Success criterion 4: `Logout` denylists the token on `/ws` (re-`Attach` fails) and on
/// `/workspace` (401).
#[tokio::test]
async fn logout_invalidates_on_ws_and_workspace() {
    let server = start_users().await;
    let mut ws = connect(request(server.port, "")).await;
    hello(&mut ws).await;
    let logged_in = login(&mut ws).await;
    let token = logged_in["access_token"].as_str().unwrap().to_string();
    let ready = next_json(&mut ws).await;
    assert_eq!(ready["type"], "ready");

    // Logout → LoggedOut, then the connection closes (it does not continue unauthenticated).
    send_cmd(&mut ws, &otto_protocol::Command::Logout).await;
    let frame = next_json(&mut ws).await;
    assert_eq!(frame["type"], "logged_out");
    assert!(
        next_json_opt(&mut ws).await.is_none(),
        "connection must close after logout"
    );

    // A re-Attach with the old token fails.
    let mut ws2 = connect(request(server.port, "")).await;
    hello(&mut ws2).await;
    send_cmd(
        &mut ws2,
        &otto_protocol::Command::Attach {
            token: token.clone(),
        },
    )
    .await;
    let err = next_json(&mut ws2).await;
    assert_eq!(err["type"], "error");
    assert_eq!(err["message"], "authentication failed");

    // /workspace with the old token is refused.
    let resp = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{}/workspace", server.port))
        .bearer_auth(&token)
        .json(&otto_protocol::WorkspaceRequest::Read {
            path: PathBuf::from("x"),
        })
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        401,
        "a denylisted token must not reach /workspace"
    );
}

/// Spec §9's serve-level replay criterion: a principal who cannot attach to another principal's
/// session gets the shared not-found error, and the other session's events are never replayed.
#[tokio::test]
async fn cross_tenant_replay_is_refused() {
    let server = start_users().await;

    // Alice's session with one replay-able event, seeded directly.
    let store = otto_persistence::SqliteStore::open(store_path(&server.dir))
        .await
        .unwrap();
    let alice_session =
        otto_persistence::SessionStore::create_session(&store, &alice(), "alice's", &json!({}))
            .await
            .unwrap();
    otto_persistence::SessionStore::append_event(
        &store,
        alice_session,
        &otto_protocol::Event {
            seq: 0,
            session: alice_session,
            kind: otto_protocol::EventKind::TurnComplete { ok: true },
        },
    )
    .await
    .unwrap();

    // Bob attaches with a token minted for him (the fake's `Login` always authenticates as its
    // fixed principal, alice; a bob token must be minted directly). The None-arm of
    // resolve_session makes a fresh session for him.
    let bob_token = server
        .fake
        .mint(&Principal { user: bob() })
        .await
        .unwrap()
        .access_token;
    let mut ws = connect(request(server.port, "")).await;
    hello(&mut ws).await;
    send_cmd(
        &mut ws,
        &otto_protocol::Command::Attach {
            token: bob_token.clone(),
        },
    )
    .await;
    let ready = next_json(&mut ws).await;
    assert_eq!(ready["type"], "ready");

    // A SendPrompt whose payload names alice's session is inert: the connection is bound to
    // bob's own session, so the turn runs there — and no event may carry alice's session id.
    send_cmd(
        &mut ws,
        &otto_protocol::Command::SendPrompt {
            session: alice_session,
            text: "x".into(),
        },
    )
    .await;
    let mut saw_turn_complete = false;
    while let Some(frame) = next_json_opt(&mut ws).await {
        if frame["type"] == "event" {
            assert_ne!(
                frame["event"]["session"].as_str().unwrap(),
                alice_session.0.to_string(),
                "no event may carry alice's session id"
            );
            if frame["event"]["kind"].get("TurnComplete").is_some() {
                saw_turn_complete = true;
                break;
            }
        }
    }
    assert!(saw_turn_complete, "bob's own-session turn must complete");

    // Bob reconnects to alice's session with last_seq=0: refused at attach, nothing replayed.
    let mut ws2 = connect(request(
        server.port,
        &format!("?session={}&last_seq=0", alice_session.0),
    ))
    .await;
    hello(&mut ws2).await;
    send_cmd(
        &mut ws2,
        &otto_protocol::Command::Attach { token: bob_token },
    )
    .await;
    let err2 = next_json(&mut ws2).await;
    assert_eq!(err2["type"], "error");
    assert_eq!(
        err2["message"]
            .as_str()
            .unwrap()
            .strip_prefix("no session "),
        Some(alice_session.0.to_string().as_str()),
        "unexpected: {err2}"
    );
    assert!(
        next_json_opt(&mut ws2).await.is_none(),
        "no Ready and no replayed events may follow a refused attach"
    );
}

/// The header pre-resolves a principal, skipping the handshake deadline — but not the `Hello`
/// greeting (which is always first).
#[tokio::test]
async fn header_pre_resolves_skipping_the_deadline() {
    let server = start_users().await;
    let token = server
        .fake
        .mint(&Principal { user: alice() })
        .await
        .unwrap()
        .access_token;
    let mut req = request(server.port, "");
    req.headers_mut()
        .insert("Authorization", format!("Bearer {token}").parse().unwrap());
    let mut ws = connect(req).await;
    let hello = hello(&mut ws).await;
    assert_eq!(hello["auth_mode"], "users");
    // No frames are sent (the deadline is far shorter than the client's patience); the header
    // carries the connection straight to Ready.
    let ready = next_json(&mut ws).await;
    assert_eq!(ready["type"], "ready");
}

/// `SingleUser`: no deadline, no auth frame — `Hello` then straight to `Ready`, every connection
/// the local principal.
#[tokio::test]
async fn single_user_goes_straight_to_ready() {
    let (port, _dir) = start_single_user().await;
    let mut ws = connect(request(port, "")).await;
    let hello = hello(&mut ws).await;
    assert_eq!(hello["auth_mode"], "single_user");
    let ready = next_json(&mut ws).await;
    assert_eq!(ready["type"], "ready");
}

/// `Machine`: the promotion secret via `Attach`, then `Ready` — and the connection adopts the
/// attached session's owner (alice's), so handover authorization passes.
#[tokio::test]
async fn machine_attach_with_the_promotion_secret_adopts_the_session_owner() {
    let (port, dir) = start_machine().await;
    let store = otto_persistence::SqliteStore::open(store_path(&dir))
        .await
        .unwrap();
    let session =
        otto_persistence::SessionStore::create_session(&store, &alice(), "promoted", &json!({}))
            .await
            .unwrap();

    let mut ws = connect(request(port, &format!("?session={}", session.0))).await;
    let hello = hello(&mut ws).await;
    assert_eq!(hello["auth_mode"], "machine");
    send_cmd(
        &mut ws,
        &otto_protocol::Command::Attach {
            token: SECRET.into(),
        },
    )
    .await;
    let ready = next_json(&mut ws).await;
    assert_eq!(ready["type"], "ready");
    assert_eq!(ready["session"].as_str().unwrap(), session.0.to_string());

    // The connection adopted alice's ownership: handover authorization clears (the failure below
    // is the absent promote config, never a "no session" refusal).
    send_cmd(
        &mut ws,
        &otto_protocol::Command::PromoteToRemote { session },
    )
    .await;
    let frame = next_json(&mut ws).await;
    assert_eq!(frame["type"], "error");
    assert!(
        !frame["message"].as_str().unwrap().contains("no session"),
        "the adopted owner must clear authorization: {frame}"
    );
}

/// `Machine` creates no sessions: an `Attach` without `?session=` has no owner to adopt and is
/// rejected, leaving the store empty.
#[tokio::test]
async fn machine_rejects_session_creation() {
    let (port, dir) = start_machine().await;
    let mut ws = connect(request(port, "")).await;
    let hello = hello(&mut ws).await;
    assert_eq!(hello["auth_mode"], "machine");
    send_cmd(
        &mut ws,
        &otto_protocol::Command::Attach {
            token: SECRET.into(),
        },
    )
    .await;
    let frame = next_json(&mut ws).await;
    assert_eq!(frame["type"], "error");
    assert!(
        frame["message"]
            .as_str()
            .unwrap()
            .contains("machine receivers do not create sessions"),
        "unexpected: {frame}"
    );
    assert_eq!(
        session_count(&dir).await,
        0,
        "a Machine receiver must not create sessions"
    );
}

/// §7.2's per-command re-verification: a token revoked after the handshake is rejected on the
/// next command. The `SendPrompt` gets the opaque error and is NOT dispatched (no session event
/// frame), and the connection stays open — a subsequent `Refresh` round-trips a fresh pair.
#[tokio::test]
async fn per_command_reverification_rejects_a_revoked_token() {
    let server = start_users().await;
    let mut ws = connect(request(server.port, "")).await;
    let hello = hello(&mut ws).await;
    assert_eq!(hello["auth_mode"], "users");
    let logged_in = login(&mut ws).await;
    let access_token = logged_in["access_token"].as_str().unwrap().to_string();
    let refresh_token = logged_in["refresh_token"].as_str().unwrap().to_string();
    let ready = next_json(&mut ws).await;
    assert_eq!(ready["type"], "ready");
    let session = otto_protocol::SessionId(
        uuid::Uuid::parse_str(ready["session"].as_str().unwrap()).unwrap(),
    );

    // Revoke the token while the connection is live, then try to run a turn.
    server.fake.logout(&access_token).await.unwrap();
    send_cmd(
        &mut ws,
        &otto_protocol::Command::SendPrompt {
            session,
            text: "add a greeting".into(),
        },
    )
    .await;
    let frame = next_json(&mut ws).await;
    assert_eq!(frame["type"], "error");
    assert_eq!(frame["message"], "authentication failed");

    // The command is not dispatched, and the socket is NOT closed by the re-verify failure: a
    // `Refresh` (which rides ahead of the re-verify) rotates to a fresh pair, proving the
    // connection survived to recover. No event frame may arrive before that `LoggedIn`.
    send_cmd(
        &mut ws,
        &otto_protocol::Command::Refresh {
            refresh_token: refresh_token.clone(),
        },
    )
    .await;
    loop {
        let frame = next_json(&mut ws).await;
        assert_ne!(
            frame["type"], "event",
            "a revoked-token SendPrompt must not be dispatched"
        );
        if frame["type"] == "logged_in" {
            assert_ne!(
                frame["access_token"].as_str().unwrap(),
                access_token,
                "refresh must rotate the access token"
            );
            break;
        }
    }
}

/// `Refresh` → `LoggedIn` round-trips on the wire: a fresh pair arrives with DIFFERENT access and
/// refresh tokens, and the connection is re-bound to the new access token (a subsequent command
/// is accepted). The fake's `rotate_refresh` consumes the old refresh token but leaves the old
/// access token verifying, so the rebind is proven by the new token working, not the old one
/// failing.
#[tokio::test]
async fn refresh_rotates_and_rebinds() {
    let server = start_users().await;
    let mut ws = connect(request(server.port, "")).await;
    let hello = hello(&mut ws).await;
    assert_eq!(hello["auth_mode"], "users");
    let logged_in = login(&mut ws).await;
    let access_token = logged_in["access_token"].as_str().unwrap().to_string();
    let refresh_token = logged_in["refresh_token"].as_str().unwrap().to_string();
    let ready = next_json(&mut ws).await;
    assert_eq!(ready["type"], "ready");
    let session = otto_protocol::SessionId(
        uuid::Uuid::parse_str(ready["session"].as_str().unwrap()).unwrap(),
    );

    send_cmd(
        &mut ws,
        &otto_protocol::Command::Refresh {
            refresh_token: refresh_token.clone(),
        },
    )
    .await;
    let refreshed = next_json(&mut ws).await;
    assert_eq!(refreshed["type"], "logged_in");
    let new_access = refreshed["access_token"].as_str().unwrap().to_string();
    let new_refresh = refreshed["refresh_token"].as_str().unwrap().to_string();
    assert_ne!(
        new_access, access_token,
        "refresh must rotate the access token"
    );
    assert_ne!(
        new_refresh, refresh_token,
        "refresh must rotate the refresh token"
    );

    // The connection is now bound to the rotated token: a SendPrompt on it is accepted and runs
    // to completion (the re-verify uses the new token, so an accepted turn proves the rebind).
    send_cmd(
        &mut ws,
        &otto_protocol::Command::SendPrompt {
            session,
            text: "add a greeting".into(),
        },
    )
    .await;
    let mut saw_turn_complete = false;
    while let Some(frame) = next_json_opt(&mut ws).await {
        if frame["type"] == "event" && frame["event"]["kind"].get("TurnComplete").is_some() {
            saw_turn_complete = true;
            break;
        }
        assert_ne!(
            frame["type"], "error",
            "a command on the rotated token must be accepted: {frame}"
        );
    }
    assert!(
        saw_turn_complete,
        "a SendPrompt on the rotated token must run to TurnComplete"
    );
}

/// A wrong promotion secret on a `Machine` receiver is the single opaque error (A11), then the
/// connection closes — no cause-specific detail leaks.
#[tokio::test]
async fn machine_wrong_secret_is_the_opaque_error() {
    let (port, _dir) = start_machine().await;
    let mut ws = connect(request(port, "")).await;
    let hello = hello(&mut ws).await;
    assert_eq!(hello["auth_mode"], "machine");

    send_cmd(
        &mut ws,
        &otto_protocol::Command::Attach {
            token: "wrong-secret".into(),
        },
    )
    .await;
    let frame = next_json(&mut ws).await;
    assert_eq!(frame["type"], "error");
    assert_eq!(frame["message"], "authentication failed");
    assert!(
        next_json_opt(&mut ws).await.is_none(),
        "connection must close after a failed machine attach"
    );
}

/// The promotion secret via the frame `Attach` reaches `Ready` on a `Machine` receiver — the
/// header-less path (§7.2). The receiver adopts an existing session (it creates none).
#[tokio::test]
async fn machine_attach_with_the_promotion_secret_reaches_ready() {
    let (port, dir) = start_machine().await;
    let store = otto_persistence::SqliteStore::open(store_path(&dir))
        .await
        .unwrap();
    let session =
        otto_persistence::SessionStore::create_session(&store, &alice(), "promoted", &json!({}))
            .await
            .unwrap();

    let mut ws = connect(request(port, &format!("?session={}", session.0))).await;
    let hello = hello(&mut ws).await;
    assert_eq!(hello["auth_mode"], "machine");
    send_cmd(
        &mut ws,
        &otto_protocol::Command::Attach {
            token: SECRET.into(),
        },
    )
    .await;
    // Attach sends no LoggedIn on Machine; the next frame is Ready.
    let ready = next_json(&mut ws).await;
    assert_eq!(ready["type"], "ready");
    assert_eq!(ready["session"].as_str().unwrap(), session.0.to_string());
    assert_eq!(
        session_count(&dir).await,
        1,
        "a machine attach must not create a session"
    );
}

/// §7.2's re-verification covers the frames `run_turn_loop` consumes mid-turn, not just the main
/// loop's dispatch: a revoked token cannot approve a gated edit. The approval-mode registry
/// parks the turn on an `fs.write` `Ask`, so the `ApproveDiff` arrives while the turn is
/// definitively in flight; the re-verify failure aborts the turn and closes the socket.
#[tokio::test]
async fn revoked_token_cannot_approve_mid_turn() {
    let server = start_users_approving().await;
    let mut ws = connect(request(server.port, "")).await;
    let hello = hello(&mut ws).await;
    assert_eq!(hello["auth_mode"], "users");
    let logged_in = login(&mut ws).await;
    let access_token = logged_in["access_token"].as_str().unwrap().to_string();
    let ready = next_json(&mut ws).await;
    assert_eq!(ready["type"], "ready");
    let session = otto_protocol::SessionId(
        uuid::Uuid::parse_str(ready["session"].as_str().unwrap()).unwrap(),
    );

    // Start a turn whose Coder edit asks for approval; it parks on the gated write. Any event
    // frame proves the turn started (and the socket reader is consuming in-turn frames).
    send_cmd(
        &mut ws,
        &otto_protocol::Command::SendPrompt {
            session,
            text: "add a greeting".into(),
        },
    )
    .await;
    loop {
        let frame = next_json(&mut ws).await;
        if frame["type"] == "event" {
            break;
        }
    }

    // Revoke the token mid-turn: the next in-turn command must fail closed rather than approving
    // the parked edit. Pre-approval events may still stream before the ApproveDiff is processed;
    // the failure then aborts the turn and closes the socket.
    server.fake.logout(&access_token).await.unwrap();
    send_cmd(
        &mut ws,
        &otto_protocol::Command::ApproveDiff {
            session,
            id: uuid::Uuid::new_v4(),
            approved: true,
        },
    )
    .await;

    loop {
        let frame = next_json(&mut ws).await;
        match frame["type"].as_str() {
            Some("error") => {
                assert_eq!(frame["message"], "authentication failed");
                break;
            }
            Some("event") => {
                assert!(
                    frame["event"]["kind"].get("FileEdit").is_none(),
                    "the parked edit must not be applied: {frame}"
                );
            }
            other => panic!("unexpected frame type after mid-turn auth failure: {other:?}"),
        }
    }
    assert!(
        next_json_opt(&mut ws).await.is_none(),
        "the connection must close after the mid-turn auth failure (aborting the turn)"
    );
}
