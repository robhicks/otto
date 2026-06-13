//! End-to-end: a full turn writes the generated file into the workspace and emits a
//! sequenced event stream ending in a successful TurnComplete.

use std::path::{Path, PathBuf};

use otto_engine::run_goal;
use otto_engine_core::traits::Workspace;
use otto_protocol::EventKind;
use otto_providers::LocalProvider;
use otto_workspace::LocalWorkspace;

#[tokio::test]
async fn full_turn_writes_output_file_and_completes_ok() {
    let dir = tempfile::tempdir().unwrap();
    let provider = LocalProvider::new();
    let workspace = LocalWorkspace::new(dir.path());

    let (events, outcome) = run_goal("add a greeting", &provider, &workspace)
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
