//! End-to-end: a full turn drives the real Planner + Coder against a scripted model that
//! returns structured JSON, writes the parsed edit into the workspace, and emits a sequenced
//! event stream ending in a successful TurnComplete.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use otto_engine::{build_tool_registry, run_goal};
use otto_engine_core::traits::Workspace;
use otto_protocol::EventKind;
use otto_providers::ScriptedProvider;
use otto_router::SingleProviderRouter;
use otto_workspace::LocalWorkspace;

#[tokio::test]
async fn full_turn_writes_parsed_edit_and_completes_ok() {
    let dir = tempfile::tempdir().unwrap();

    // A scripted model: the planner prompt contains "milestones", the coder prompt contains
    // "edits". First matching rule wins, so list "edits" first (the coder prompt does not
    // contain "milestones", and the planner prompt does not contain "edits").
    let provider = ScriptedProvider::new("{}")
        .on(
            "edits",
            r#"{"edits": [{"path": "otto_output.txt", "contents": "Hello! add a greeting"}]}"#,
        )
        .on(
            "milestones",
            r#"{"milestones": [{"description": "write the greeting"}]}"#,
        );
    let router: Arc<dyn otto_engine_core::Router> =
        Arc::new(SingleProviderRouter::new(Arc::new(provider)));
    let workspace: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(dir.path()));
    let tools_workspace: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(dir.path()));
    let tools = Arc::new(build_tool_registry(tools_workspace, dir.path().to_path_buf()));
    let store: Arc<dyn otto_persistence::SessionStore> =
        Arc::new(otto_persistence::SqliteStore::open(dir.path().join("sessions.db")).await.unwrap());

    let (events, outcome) = run_goal("add a greeting", store, router, workspace.clone(), tools)
        .await
        .unwrap();

    assert!(outcome.ok);
    assert_eq!(
        events.last().unwrap().kind,
        EventKind::TurnComplete { ok: true }
    );

    for (i, event) in events.iter().enumerate() {
        assert_eq!(event.seq, i as u64);
    }

    // The Coder's PARSED edit was applied (it passed the gate — otto_output.txt is not sensitive).
    let written = workspace.read(Path::new("otto_output.txt")).await.unwrap();
    let text = String::from_utf8(written).unwrap();
    assert!(text.contains("add a greeting"));

    assert!(events.iter().any(|e| matches!(
        &e.kind,
        EventKind::FileEdit { path, .. } if path == &PathBuf::from("otto_output.txt")
    )));
}
