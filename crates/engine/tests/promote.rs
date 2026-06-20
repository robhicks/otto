//! Capstone: run a turn on a source engine, promote the session to a LoopbackTarget (a real
//! second in-process engine), then reconnect a WS client to the provisioned remote and confirm
//! the session resumed (same id, replayed event gap) and the workspace transferred. Loopback only.

use std::path::Path;
use std::sync::Arc;

use futures_util::StreamExt;
use otto_engine::{
    CollectingSink, EngineService, LoopbackTarget, RemoteTarget, build_default_registry,
    build_tool_registry, promote,
};
use otto_engine_core::traits::{Workspace, WorkspaceRead};
use otto_persistence::{SessionStore, SqliteStore};
use otto_providers::ScriptedProvider;
use otto_router::SingleProviderRouter;
use otto_workspace::{LocalWorkspace, RemoteWorkspace};
use serde_json::Value;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

const TOKEN: &str = "promote-token";

async fn next_json(
    ws: &mut (impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin),
) -> Option<Value> {
    loop {
        match ws.next().await {
            Some(Ok(Message::Text(t))) => return Some(serde_json::from_str(t.as_str()).unwrap()),
            Some(Ok(Message::Close(_))) | None => return None,
            Some(Ok(_)) => continue,
            Some(Err(_)) => return None,
        }
    }
}

#[tokio::test]
async fn promote_resumes_session_and_workspace_on_a_loopback_remote() {
    // --- Source engine: run a turn that writes a file. ---
    let src_ws_dir = tempfile::tempdir().unwrap();
    let src_db_dir = tempfile::tempdir().unwrap();
    let promote_base = tempfile::tempdir().unwrap();

    let provider = ScriptedProvider::new("{}")
        .on(
            "edits",
            r#"{"edits": [{"path": "out.txt", "contents": "PROMOTED"}]}"#,
        )
        .on("milestones", r#"{"milestones": [{"description": "x"}]}"#);
    let router: Arc<dyn otto_engine_core::Router> =
        Arc::new(SingleProviderRouter::new(Arc::new(provider)));
    let workspace: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(src_ws_dir.path()));
    let tools_ws: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(src_ws_dir.path()));
    let tools = Arc::new(build_tool_registry(
        tools_ws,
        src_ws_dir.path().to_path_buf(),
    ));
    let store: Arc<dyn SessionStore> = Arc::new(
        SqliteStore::open(src_db_dir.path().join("a.db"))
            .await
            .unwrap(),
    );
    let service = EngineService::new(
        store.clone(),
        Arc::new(build_default_registry()),
        router,
        workspace.clone(),
        tools,
    );

    let session = service
        .create_session("g", &serde_json::json!({}))
        .await
        .unwrap();
    let mut sink = CollectingSink::default();
    service
        .run_prompt(session, "add a greeting", &mut sink)
        .await
        .unwrap();
    let src_events = store.replay_since(session, None).await.unwrap();
    assert!(
        src_events.len() >= 2,
        "the source turn should emit several events"
    );
    let last_seq = src_events.last().unwrap().seq;

    // --- Promote to a loopback remote. ---
    let target = LoopbackTarget::new(TOKEN, promote_base.path().to_path_buf(), true);
    let handle = promote(&*store, &*workspace, session, &target)
        .await
        .unwrap();

    // --- Reconnect to the remote: same session, replayed gap after seq 0. ---
    let url = format!("{}/ws?session={}&last_seq=0", handle.endpoint, session.0);
    let mut req = url.into_client_request().unwrap();
    req.headers_mut()
        .insert("Authorization", format!("Bearer {TOKEN}").parse().unwrap());
    let (mut ws, _) = tokio_tungstenite::connect_async(req)
        .await
        .expect("connect to remote");

    let ready = next_json(&mut ws).await.expect("ready frame");
    assert_eq!(ready["type"], "ready");
    assert_eq!(ready["session"].as_str().unwrap(), session.0.to_string());

    let mut replayed = Vec::new();
    while let Some(frame) = next_json(&mut ws).await {
        if frame["type"] == "event" {
            let seq = frame["event"]["seq"].as_u64().unwrap();
            replayed.push(seq);
            if seq == last_seq {
                break;
            }
        }
    }
    // The gap after seq 0 is every source event with seq > 0.
    let expected: Vec<u64> = src_events
        .iter()
        .map(|e| e.seq)
        .filter(|s| *s > 0)
        .collect();
    assert_eq!(replayed, expected);
    drop(ws);

    // --- The workspace transferred: read the promoted file via the remote's /workspace RPC. ---
    let http_base = handle.endpoint.replace("ws://", "http://");
    let remote_ws = RemoteWorkspace::new(http_base, TOKEN);
    assert_eq!(
        remote_ws.read(Path::new("out.txt")).await.unwrap(),
        b"PROMOTED"
    );

    // --- Teardown stops the remote: a subsequent connect fails. ---
    let endpoint = handle.endpoint.clone();
    target.teardown(handle).await.unwrap();
    tokio::task::yield_now().await;
    let down_url = format!("{endpoint}/ws");
    let mut down_req = down_url.into_client_request().unwrap();
    down_req
        .headers_mut()
        .insert("Authorization", format!("Bearer {TOKEN}").parse().unwrap());
    assert!(
        tokio_tungstenite::connect_async(down_req).await.is_err(),
        "the remote must be unreachable after teardown"
    );
}
