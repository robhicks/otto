//! microVM Provisioner seam, exercised against an in-process `otto serve --accept-promotions` on an
//! ephemeral loopback port (no hypervisor). Proves the MicrovmTarget composition end to end.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use otto_engine::{
    EngineService, MicrovmTarget, PromoteBundle, ProvisionedMachine, Provisioner, RemoteTarget,
    UnsupportedProvisioner, build_default_registry, build_tool_registry, export_bundle, serve_app,
    serve_run,
};
use otto_engine_core::auth::AuthConfig;
use otto_engine_core::traits::Workspace;
use otto_engine_core::types::WorkspaceSnapshot;
use otto_persistence::{SessionState, SessionStatus, SqliteStore};
use otto_protocol::{AuthMode, CapabilitiesManifest, SessionId};
use otto_workspace::LocalWorkspace;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

const TOKEN: &str = "microvm-token";

fn caps() -> CapabilitiesManifest {
    CapabilitiesManifest {
        engine_remote: false,
        local_llm: false,
        remote_llm: false,
        sandbox: false,
    }
}

fn sample_bundle(id: SessionId, files: Vec<(&str, &[u8])>) -> PromoteBundle {
    PromoteBundle {
        session: SessionState {
            id,
            owner: otto_protocol::UserId::local(),
            goal: "g".to_string(),
            status: SessionStatus::Active,
            config: serde_json::json!({}),
            events: vec![],
            turns: vec![],
        },
        workspace: WorkspaceSnapshot {
            files: files
                .into_iter()
                .map(|(p, b)| (PathBuf::from(p), b.to_vec()))
                .collect(),
        },
    }
}

/// A `Provisioner` that boots an in-process `otto serve --accept-promotions` on `127.0.0.1:0` — the
/// CI stand-in for a real microVM. The serve task IS the disposal handle (abort stops serving).
struct TestServeProvisioner {
    // Tempdirs are retained for the provisioner's lifetime so the booted serve's store/workspace
    // outlive provisioning; the test body keeps the provisioner alive.
    _ws: tempfile::TempDir,
    _db: tempfile::TempDir,
    endpoint: String,
    listener: std::sync::Mutex<Option<std::net::TcpListener>>,
}

impl TestServeProvisioner {
    fn new() -> Self {
        let ws = tempfile::tempdir().unwrap();
        let db = tempfile::tempdir().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        Self {
            _ws: ws,
            _db: db,
            endpoint: format!("ws://127.0.0.1:{port}"),
            listener: std::sync::Mutex::new(Some(listener)),
        }
    }

    fn http_base(&self) -> String {
        self.endpoint.replace("ws://", "http://")
    }
}

#[async_trait]
impl Provisioner for TestServeProvisioner {
    async fn provision(&self) -> anyhow::Result<ProvisionedMachine> {
        let ws_path = self._ws.path().to_path_buf();
        let workspace: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(&ws_path));
        let tools_ws: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(&ws_path));
        let tools = Arc::new(build_tool_registry(tools_ws, ws_path.clone()));
        let store: Arc<dyn otto_persistence::SessionStore> = Arc::new(
            SqliteStore::open(self._db.path().join("r.db"))
                .await
                .unwrap(),
        );
        let service = EngineService::new(
            store,
            Arc::new(build_default_registry()),
            Arc::from(otto_engine::build_router()),
            workspace,
            tools,
        );
        // The provisioned serve is a `--promotion-receiver` (`AuthMode::Machine`): accept
        // promotions is true so /promote is live. A fresh per-session secret is minted per
        // provision (spec §1.2) and doubles as the machine's promotion secret — one session per
        // machine, so the machine secret IS the session secret (spec A3).
        let secret = otto_engine::mint_session_secret();
        let auth = AuthConfig {
            mode: AuthMode::Machine,
            authenticator: None,
            promotion_secret: Some(secret.clone()),
            handshake_deadline: std::time::Duration::from_secs(10),
        };
        let app = serve_app(service, auth, caps(), None, true);
        let listener = self
            .listener
            .lock()
            .unwrap()
            .take()
            .expect("provision once");
        let task = tokio::spawn(async move {
            serve_run(listener, app, None).await.unwrap();
        });
        Ok(ProvisionedMachine {
            endpoint: self.endpoint.clone(),
            token: secret,
            task,
        })
    }
}

#[tokio::test]
async fn microvm_target_seam_round_trip() {
    let provisioner = Arc::new(TestServeProvisioner::new());
    let http_base = provisioner.http_base();
    let target = MicrovmTarget::new(provisioner.clone());

    let id = SessionId::new();
    let bundle = sample_bundle(id, vec![("out.txt", b"HELLO")]);
    let handle = target.provision(&bundle).await.unwrap();
    assert_eq!(handle.endpoint, provisioner.endpoint);
    assert!(
        handle.token.len() == 32 && handle.token.chars().all(|c| c.is_ascii_hexdigit()),
        "the microvm handle token must be a fresh 32-hex mint, got {:?}",
        handle.token
    );
    assert_ne!(
        handle.token, TOKEN,
        "the machine's session secret must not be the shared TOKEN constant"
    );

    // Prove the restore landed: export the session back off the provisioned serve (authorized by
    // the machine's session secret — its own token — never TOKEN) and check the file.
    let resp = reqwest::Client::new()
        .post(format!("{http_base}/export"))
        .bearer_auth(&handle.token)
        .json(&serde_json::json!({ "session": id.0.to_string() }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let restored: PromoteBundle = resp.json().await.unwrap();
    assert_eq!(restored.session.id, id);
    assert!(
        restored
            .workspace
            .files
            .iter()
            .any(|(p, b)| p == &PathBuf::from("out.txt") && b == b"HELLO"),
        "restored workspace should contain out.txt: {:?}",
        restored.workspace.files
    );

    // Keep `handle` alive until here so its task (the serve) is not aborted mid-assertion.
    drop(handle);
}

#[tokio::test]
async fn microvm_demote_pull_then_dispose() {
    // Provision an in-process serve (the CI stand-in for a microVM), promote a session into it,
    // then pull it back via the shared export_bundle primitive and dispose the machine.
    let provisioner = Arc::new(TestServeProvisioner::new());
    let endpoint = provisioner.endpoint.clone();
    let http_base = provisioner.http_base();
    let target = MicrovmTarget::new(provisioner.clone());

    let id = SessionId::new();
    let bundle = sample_bundle(id, vec![("pulled.txt", b"FROM_VM")]);
    let handle = target.provision(&bundle).await.unwrap();
    assert_eq!(handle.endpoint, endpoint);
    assert!(
        handle.token.len() == 32 && handle.token.chars().all(|c| c.is_ascii_hexdigit()),
        "the microvm handle token must be a fresh 32-hex mint, got {:?}",
        handle.token
    );
    assert_ne!(
        handle.token, TOKEN,
        "the handle's secret must be the fresh mint, not the shared TOKEN constant"
    );

    // Pull the session back off the provisioned serve using the handle's endpoint + token.
    let pulled = export_bundle(&handle.endpoint, &handle.token, id)
        .await
        .unwrap();
    assert_eq!(pulled.session.id, id);
    assert!(
        pulled
            .workspace
            .files
            .iter()
            .any(|(p, b)| p == &PathBuf::from("pulled.txt") && b == b"FROM_VM"),
        "pulled workspace should contain pulled.txt: {:?}",
        pulled.workspace.files
    );

    // Dispose: dropping the handle aborts the serve task → the endpoint stops listening.
    let handle_token = handle.token.clone();
    drop(handle);
    tokio::task::yield_now().await;
    let result = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(500))
        .build()
        .unwrap()
        .post(format!("{http_base}/export"))
        .bearer_auth(&handle_token)
        .json(&serde_json::json!({ "session": id.0.to_string() }))
        .send()
        .await;
    assert!(
        result.is_err(),
        "serve should be unreachable after the handle is dropped"
    );
}

#[tokio::test]
async fn microvm_target_teardown_stops_the_machine() {
    let provisioner = Arc::new(TestServeProvisioner::new());
    let http_base = provisioner.http_base();
    let target = MicrovmTarget::new(provisioner.clone());

    let bundle = sample_bundle(SessionId::new(), vec![]);
    let handle = target.provision(&bundle).await.unwrap();
    let handle_token = handle.token.clone();

    // Teardown aborts the serve task → the endpoint stops listening.
    target.teardown(handle).await.unwrap();
    // Give the abort a moment to drop the listener.
    tokio::task::yield_now().await;

    let result = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(500))
        .build()
        .unwrap()
        .post(format!("{http_base}/promote"))
        .bearer_auth(&handle_token)
        .json(&sample_bundle(SessionId::new(), vec![]))
        .send()
        .await;
    assert!(
        result.is_err(),
        "serve should be unreachable after teardown"
    );
}

#[tokio::test]
async fn microvm_target_over_unsupported_provisioner_errs() {
    let target = MicrovmTarget::new(Arc::new(UnsupportedProvisioner));
    let bundle = sample_bundle(SessionId::new(), vec![]);
    let result = target.provision(&bundle).await;
    let err = match result {
        Err(e) => e.to_string(),
        Ok(_) => panic!("provision should fail with UnsupportedProvisioner"),
    };
    assert!(err.contains("microVM provisioning requires"), "{err}");
}

fn authed_ws_request(url: &str) -> tokio_tungstenite::tungstenite::handshake::client::Request {
    let mut req = url.to_string().into_client_request().unwrap();
    req.headers_mut()
        .insert("Authorization", format!("Bearer {TOKEN}").parse().unwrap());
    req
}

/// Read the `Hello` greeting (always the first frame, in every mode).
async fn hello_frame(
    ws: &mut (impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin),
) {
    let hello = next_json(ws).await.expect("hello frame");
    assert_eq!(hello["type"], "hello");
}

/// Read `Hello` then `Ready`, returning the `Ready` frame.
async fn ready_frame(
    ws: &mut (impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin),
) -> serde_json::Value {
    hello_frame(ws).await;
    let ready = next_json(ws).await.expect("ready frame");
    assert_eq!(ready["type"], "ready");
    ready
}

async fn next_json(
    ws: &mut (impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin),
) -> Option<serde_json::Value> {
    loop {
        match ws.next().await {
            Some(Ok(Message::Text(t))) => return Some(serde_json::from_str(t.as_str()).unwrap()),
            Some(Ok(Message::Close(_))) | None => return None,
            Some(Ok(_)) => continue,
            Some(Err(_)) => return None,
        }
    }
}

/// Start a source serve in Microvm promote mode (config paths are dummy: without the firecracker
/// feature the provisioner is Unsupported and never reads them). Returns its ws base.
async fn start_source_microvm() -> (String, tempfile::TempDir, tempfile::TempDir) {
    let ws_dir = tempfile::tempdir().unwrap();
    let db_dir = tempfile::tempdir().unwrap();
    let workspace: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(ws_dir.path()));
    let tools_ws: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(ws_dir.path()));
    let tools = Arc::new(build_tool_registry(tools_ws, ws_dir.path().to_path_buf()));
    let store: Arc<dyn otto_persistence::SessionStore> =
        Arc::new(SqliteStore::open(db_dir.path().join("s.db")).await.unwrap());
    let service = EngineService::new(
        store,
        Arc::new(build_default_registry()),
        Arc::from(otto_engine::build_router()),
        workspace,
        tools,
    );
    let config = otto_engine::MicrovmConfig {
        kernel: PathBuf::from("/nonexistent/vmlinux"),
        rootfs: PathBuf::from("/nonexistent/rootfs"),
        fc_bin: PathBuf::from("/nonexistent/firecracker"),
        tap: "fc-tap0".to_string(),
        guest_ip: "172.16.0.2".to_string(),
        port: 7878,
        vcpus: 2,
        mem_mib: 1024,
        boot_timeout: std::time::Duration::from_secs(5),
    };
    let promote = Some(otto_engine::PromoteConfig {
        token: TOKEN.to_string(),
        mode: otto_engine::PromoteMode::Microvm { config },
    });
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let base = format!("ws://127.0.0.1:{port}");
    // The source's own posture is `SingleUser` — this harness drives it only through its WS
    // handover commands, never a credential (its handover token is the receiver's promotion
    // secret, carried by the `PromoteConfig`).
    let auth = AuthConfig {
        mode: AuthMode::SingleUser,
        authenticator: None,
        promotion_secret: None,
        handshake_deadline: std::time::Duration::from_secs(10),
    };
    let app = otto_engine::serve_app_with_base(service, auth, caps(), promote, false, base.clone());
    tokio::spawn(async move {
        serve_run(listener, app, None).await.unwrap();
    });
    (base, ws_dir, db_dir)
}

#[cfg(not(feature = "firecracker"))]
#[tokio::test]
async fn handover_microvm_promote_is_unsupported_without_feature() {
    let (src_ws, _w, _d) = start_source_microvm().await;
    let (mut ws, _) = tokio_tungstenite::connect_async(authed_ws_request(&format!("{src_ws}/ws")))
        .await
        .unwrap();
    let ready = ready_frame(&mut ws).await;
    let session = ready["session"].as_str().unwrap().to_string();

    let promote = serde_json::json!({ "PromoteToRemote": { "session": session } });
    ws.send(Message::Text(serde_json::to_string(&promote).unwrap()))
        .await
        .unwrap();
    loop {
        let f = next_json(&mut ws).await.expect("frame");
        if f["type"] == "error" {
            assert!(
                f["message"]
                    .as_str()
                    .unwrap()
                    .contains("microVM provisioning requires"),
                "{f:?}"
            );
            break;
        }
        assert_ne!(
            f["type"], "promoted",
            "promote must not succeed without firecracker: {f:?}"
        );
    }
}

#[tokio::test]
async fn handover_microvm_demote_without_prior_promote_errs() {
    // No promote has run, so there is no live microVM handle to pull from: demote errors honestly
    // instead of provisioning anything. (With the firecracker feature a real promote would create
    // the handle; that round-trip is not CI-able, same boundary as the promote-unsupported test.)
    let (src_ws, _w, _d) = start_source_microvm().await;
    let (mut ws, _) = tokio_tungstenite::connect_async(authed_ws_request(&format!("{src_ws}/ws")))
        .await
        .unwrap();
    let ready = ready_frame(&mut ws).await;
    let session = ready["session"].as_str().unwrap().to_string();

    let demote = serde_json::json!({ "DemoteToLocal": { "session": session } });
    ws.send(Message::Text(serde_json::to_string(&demote).unwrap()))
        .await
        .unwrap();
    loop {
        let f = next_json(&mut ws).await.expect("frame");
        if f["type"] == "error" {
            assert!(
                f["message"]
                    .as_str()
                    .unwrap()
                    .contains("no active microvm handover"),
                "{f:?}"
            );
            break;
        }
        assert_ne!(
            f["type"], "demoted",
            "demote must not succeed with no prior promote: {f:?}"
        );
    }
}
