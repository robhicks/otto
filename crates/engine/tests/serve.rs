//! End-to-end: the axum WebSocket server streams a turn's events to a connected client,
//! supports Last-Event-ID reconnect, and rejects unauthenticated connections. Runs on a
//! loopback ephemeral port — no external network.

use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use otto_engine::{EngineService, build_default_registry, build_tool_registry, serve_app};
use otto_engine_core::traits::Workspace;
use otto_providers::ScriptedProvider;
use otto_router::SingleProviderRouter;
use otto_workspace::LocalWorkspace;
use serde_json::Value;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

const TOKEN: &str = "test-token";

/// Start the serve app on 127.0.0.1:0 and return the bound port. Keeps the tempdir alive.
async fn start_server() -> (u16, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
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
    let tools = Arc::new(build_tool_registry(tools_ws, dir.path().to_path_buf()));
    let store: Arc<dyn otto_persistence::SessionStore> = Arc::new(
        otto_persistence::SqliteStore::open(dir.path().join("s.db"))
            .await
            .unwrap(),
    );
    let service = EngineService::new(
        store,
        Arc::new(build_default_registry()),
        router,
        workspace,
        tools,
    );

    let app = serve_app(service, TOKEN.to_string());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (port, dir)
}

fn authed_request(
    port: u16,
    query: &str,
) -> tokio_tungstenite::tungstenite::handshake::client::Request {
    let url = format!("ws://127.0.0.1:{port}/ws{query}");
    let mut req = url.into_client_request().unwrap();
    req.headers_mut()
        .insert("Authorization", format!("Bearer {TOKEN}").parse().unwrap());
    req
}

#[tokio::test]
async fn streams_a_turn_then_reconnects_with_replay() {
    let (port, _dir) = start_server().await;

    // First connection: new session, send a prompt, collect streamed frames.
    let (mut ws, _) = tokio_tungstenite::connect_async(authed_request(port, ""))
        .await
        .expect("connect");

    // First frame is Ready { session }.
    let ready: Value = next_json(&mut ws).await;
    assert_eq!(ready["type"], "ready");
    let session = ready["session"].as_str().unwrap().to_string();

    // Send a prompt for that session.
    let cmd = serde_json::json!({ "SendPrompt": { "session": session, "text": "add a greeting" } });
    ws.send(Message::Text(serde_json::to_string(&cmd).unwrap()))
        .await
        .unwrap();

    // Collect event frames until TurnComplete.
    let mut seqs = Vec::new();
    let mut saw_turn_complete = false;
    while let Some(frame) = next_json_opt(&mut ws).await {
        if frame["type"] == "event" {
            let kind = &frame["event"]["kind"];
            seqs.push(frame["event"]["seq"].as_u64().unwrap());
            if kind.get("TurnComplete").is_some() {
                saw_turn_complete = true;
                break;
            }
        }
    }
    assert!(saw_turn_complete, "expected a TurnComplete event");
    assert_eq!(seqs.first(), Some(&0), "events start at seq 0");
    let last_seq = *seqs.last().unwrap();
    drop(ws);

    // Reconnect with last_seq = 0: expect the gap (events with seq > 0) replayed.
    let (mut ws2, _) = tokio_tungstenite::connect_async(authed_request(
        port,
        &format!("?session={session}&last_seq=0"),
    ))
    .await
    .expect("reconnect");
    let ready2: Value = next_json(&mut ws2).await;
    assert_eq!(ready2["type"], "ready");
    assert_eq!(ready2["session"].as_str().unwrap(), session);

    let mut replayed = Vec::new();
    // The gap is finite; read until we've seen the last seq, then stop.
    while let Some(frame) = next_json_opt(&mut ws2).await {
        if frame["type"] == "event" {
            let seq = frame["event"]["seq"].as_u64().unwrap();
            replayed.push(seq);
            if seq == last_seq {
                break;
            }
        }
    }
    assert!(replayed.iter().all(|&s| s > 0), "replay gap excludes seq 0");
    assert_eq!(replayed.last(), Some(&last_seq));
}

#[tokio::test]
async fn rejects_missing_token() {
    let (port, _dir) = start_server().await;
    let url = format!("ws://127.0.0.1:{port}/ws");
    // No Authorization header → upgrade rejected (401), connect_async errors.
    let result = tokio_tungstenite::connect_async(url).await;
    assert!(
        result.is_err(),
        "unauthenticated connection must be rejected"
    );
}

#[tokio::test]
async fn rejects_wrong_token() {
    let (port, _dir) = start_server().await;
    let url = format!("ws://127.0.0.1:{port}/ws");
    let mut req = url.into_client_request().unwrap();
    req.headers_mut()
        .insert("Authorization", "Bearer wrong-token".parse().unwrap());
    let result = tokio_tungstenite::connect_async(req).await;
    assert!(result.is_err(), "a wrong token must be rejected");
}

/// Receive the next text frame as JSON (panics on close/non-text or stream end).
async fn next_json(
    ws: &mut (impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin),
) -> Value {
    next_json_opt(ws).await.expect("expected a frame")
}

/// Receive the next text frame as JSON, or None if the stream ended / a non-text frame arrived.
async fn next_json_opt(
    ws: &mut (impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin),
) -> Option<Value> {
    loop {
        match ws.next().await {
            Some(Ok(Message::Text(t))) => return Some(serde_json::from_str(t.as_str()).unwrap()),
            Some(Ok(Message::Close(_))) | None => return None,
            Some(Ok(_)) => continue, // skip ping/pong/binary
            Some(Err(_)) => return None,
        }
    }
}
