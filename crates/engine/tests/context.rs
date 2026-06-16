//! End-to-end: the ContextFinder selects a seeded file and the Coder reads its contents, so the
//! scripted model only produces the edit when the file's contents reached the Coder's prompt.

use std::path::Path;
use std::sync::Arc;

use otto_engine::{build_tool_registry, run_goal};
use otto_engine_core::traits::{Workspace, WorkspaceRead};
use otto_engine_core::types::Edit;
use otto_providers::ScriptedProvider;
use otto_router::SingleProviderRouter;
use otto_workspace::LocalWorkspace;

#[tokio::test]
async fn context_flows_from_finder_to_coder() {
    let dir = tempfile::tempdir().unwrap();
    let seed_ws = LocalWorkspace::new(dir.path());
    // Seed a file the goal's keywords match ("thing"), containing a unique marker.
    seed_ws
        .apply_edit(&Edit {
            path: std::path::PathBuf::from("src/thing.rs"),
            new_contents: "fn thing() { /* CTX_MARKER_77 */ }".to_string(),
        })
        .await
        .unwrap();

    // The coder rule fires only on the seeded file's marker — proving the ContextFinder picked
    // src/thing.rs and the Coder injected its contents. The context-finder's own select prompt
    // contains neither "CTX_MARKER_77" nor "edits", so it falls back to the lexical pick.
    let provider = ScriptedProvider::new("{}").on(
        "CTX_MARKER_77",
        r#"{"edits": [{"path": "result.txt", "contents": "used context"}]}"#,
    );
    let router = SingleProviderRouter::new(Arc::new(provider));
    let workspace = LocalWorkspace::new(dir.path());

    let tools_workspace: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(dir.path()));
    let tools = build_tool_registry(tools_workspace, dir.path().to_path_buf());

    let store = otto_persistence::SqliteStore::open(dir.path().join("sessions.db"))
        .await
        .unwrap();
    let (_events, outcome) = run_goal(
        "update the thing function",
        &store,
        &router,
        &workspace,
        &tools,
    )
    .await
    .unwrap();

    assert!(outcome.ok);
    let written = workspace.read(Path::new("result.txt")).await.unwrap();
    assert_eq!(String::from_utf8(written).unwrap(), "used context");
}
