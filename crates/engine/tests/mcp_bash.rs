//! End-to-end: build mcp-bash, spawn it via the MCP client, register its `bash` tool in a
//! ToolRegistry whose ask-resolver allows `bash`, and confirm a sandboxed echo round-trips.
//! Self-skips when no OS sandbox backend is available (matching the sandbox test pattern).

use std::sync::Arc;

use otto_engine::mcp_connect_bash;
use otto_engine_core::tool::{AllowListAskResolver, ToolRegistry};
use otto_tools::{DefaultPermissionGate, os_sandbox_available};
use serde_json::json;

#[tokio::test]
async fn mcp_bash_runs_sandboxed_echo() {
    if !os_sandbox_available() {
        eprintln!("skipping mcp_bash test: no OS sandbox backend");
        return;
    }
    let bin = escargot::CargoBuild::new()
        .package("otto-mcp-bash")
        .bin("mcp-bash")
        .run()
        .expect("build mcp-bash")
        .path()
        .to_path_buf();

    let dir = tempfile::tempdir().unwrap();
    let (_conn, tools) = mcp_connect_bash(bin.to_str().unwrap(), dir.path())
        .await
        .expect("connect to mcp-bash");
    // The gate classifies `bash` as Ask; allow it (as the engine does when sandboxed).
    let mut registry = ToolRegistry::new(
        Arc::new(DefaultPermissionGate::new()),
        Arc::new(AllowListAskResolver::new(vec!["bash".to_string()])),
    );
    for t in tools {
        registry.register(t);
    }

    let out = registry
        .call("bash", json!({ "command": "echo hi" }))
        .await
        .unwrap();
    assert!(out["stdout"].as_str().unwrap().contains("hi"));
    assert_eq!(out["exit_code"].as_i64().unwrap(), 0);
}
