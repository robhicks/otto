//! End-to-end: a full turn writes the generated file into the workspace and emits a
//! sequenced event stream ending in a successful TurnComplete.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use otto_engine::{build_tool_registry, run_goal};
use otto_engine_core::traits::Workspace;
use otto_protocol::EventKind;
use otto_providers::LocalProvider;
use otto_router::SingleProviderRouter;
use otto_workspace::LocalWorkspace;

#[tokio::test]
async fn full_turn_writes_output_file_and_completes_ok() {
    let dir = tempfile::tempdir().unwrap();
    let router = SingleProviderRouter::new(Arc::new(LocalProvider::new()));
    let workspace = LocalWorkspace::new(dir.path());

    let tools_workspace: std::sync::Arc<dyn Workspace> =
        std::sync::Arc::new(LocalWorkspace::new(dir.path()));
    let tools = build_tool_registry(tools_workspace);

    let (events, outcome) = run_goal("add a greeting", &router, &workspace, &tools)
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

    let written = workspace.read(Path::new("otto_output.txt")).await.unwrap();
    let text = String::from_utf8(written).unwrap();
    assert!(text.contains("add a greeting"));

    assert!(events.iter().any(|e| matches!(
        &e.kind,
        EventKind::FileEdit { path, .. } if path == &PathBuf::from("otto_output.txt")
    )));
}
