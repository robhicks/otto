//! microVM Provisioner seam, exercised against an in-process `otto serve --accept-promotions` on an
//! ephemeral loopback port (no hypervisor). Proves the MicrovmTarget composition end to end.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use otto_engine::{
    EngineService, MicrovmTarget, PromoteBundle, ProvisionedMachine, Provisioner, RemoteTarget,
    UnsupportedProvisioner, build_default_registry, build_tool_registry, serve_app, serve_run,
};
use otto_engine_core::traits::Workspace;
use otto_engine_core::types::WorkspaceSnapshot;
use otto_persistence::{SessionState, SessionStatus, SqliteStore};
use otto_protocol::{CapabilitiesManifest, SessionId};
use otto_workspace::LocalWorkspace;

const TOKEN: &str = "microvm-token";

fn caps() -> CapabilitiesManifest {
    CapabilitiesManifest { engine_remote: false, local_llm: false, remote_llm: false, sandbox: false }
}

fn sample_bundle(id: SessionId, files: Vec<(&str, &[u8])>) -> PromoteBundle {
    PromoteBundle {
        session: SessionState {
            id,
            goal: "g".to_string(),
            status: SessionStatus::Active,
            config: serde_json::json!({}),
            events: vec![],
            turns: vec![],
        },
        workspace: WorkspaceSnapshot {
            files: files.into_iter().map(|(p, b)| (PathBuf::from(p), b.to_vec())).collect(),
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
        let store: Arc<dyn otto_persistence::SessionStore> =
            Arc::new(SqliteStore::open(self._db.path().join("r.db")).await.unwrap());
        let service = EngineService::new(
            store,
            Arc::new(build_default_registry()),
            Arc::from(otto_engine::build_router()),
            workspace,
            tools,
        );
        // accept_promotions = true so the /promote restore RPC is live.
        let app = serve_app(service, TOKEN.to_string(), caps(), None, true);
        let listener = self.listener.lock().unwrap().take().expect("provision once");
        let task = tokio::spawn(async move {
            serve_run(listener, app, None).await.unwrap();
        });
        Ok(ProvisionedMachine { endpoint: self.endpoint.clone(), token: TOKEN.to_string(), task })
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

    // Prove the restore landed: export the session back off the provisioned serve and check the file.
    let resp = reqwest::Client::new()
        .post(format!("{http_base}/export"))
        .bearer_auth(TOKEN)
        .json(&serde_json::json!({ "session": id.0.to_string() }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let restored: PromoteBundle = resp.json().await.unwrap();
    assert_eq!(restored.session.id, id);
    assert!(
        restored.workspace.files.iter().any(|(p, b)| p == &PathBuf::from("out.txt") && b == b"HELLO"),
        "restored workspace should contain out.txt: {:?}",
        restored.workspace.files
    );

    // Keep `handle` alive until here so its task (the serve) is not aborted mid-assertion.
    drop(handle);
}

#[tokio::test]
async fn microvm_target_teardown_stops_the_machine() {
    let provisioner = Arc::new(TestServeProvisioner::new());
    let http_base = provisioner.http_base();
    let target = MicrovmTarget::new(provisioner.clone());

    let bundle = sample_bundle(SessionId::new(), vec![]);
    let handle = target.provision(&bundle).await.unwrap();

    // Teardown aborts the serve task → the endpoint stops listening.
    target.teardown(handle).await.unwrap();
    // Give the abort a moment to drop the listener.
    tokio::task::yield_now().await;

    let result = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(500))
        .build()
        .unwrap()
        .post(format!("{http_base}/promote"))
        .bearer_auth(TOKEN)
        .json(&sample_bundle(SessionId::new(), vec![]))
        .send()
        .await;
    assert!(result.is_err(), "serve should be unreachable after teardown");
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
