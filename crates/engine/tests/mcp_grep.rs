//! End-to-end: build mcp-grep, spawn it via the MCP client, register its `grep` tool in a gated
//! ToolRegistry, and confirm search works and the gate denies a sensitive path arg. Loopback (stdio).

use std::sync::Arc;

use otto_engine::mcp_connect_grep;
use otto_engine_core::tool::{DenyAsk, ToolRegistry};
use otto_tools::DefaultPermissionGate;
use serde_json::json;

#[tokio::test]
async fn mcp_grep_searches_and_stays_gated() {
    let bin = escargot::CargoBuild::new()
        .package("otto-mcp-grep")
        .bin("mcp-grep")
        .run()
        .expect("build mcp-grep")
        .path()
        .to_path_buf();

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "alpha\nTODO: wire it\n").unwrap();

    let (_conn, tools) = mcp_connect_grep(bin.to_str().unwrap(), dir.path())
        .await
        .expect("connect to mcp-grep");
    let mut registry = ToolRegistry::new(Arc::new(DefaultPermissionGate::new()), Arc::new(DenyAsk));
    for t in tools {
        registry.register(t);
    }

    // grep returns matches with the right shape.
    let out = registry
        .call("grep", json!({ "pattern": "TODO" }))
        .await
        .unwrap();
    let matches = out["matches"].as_array().expect("matches array");
    assert!(
        matches
            .iter()
            .any(|m| m["path"] == "a.txt" && m["line"].as_str().unwrap().contains("TODO"))
    );
    assert_eq!(out["truncated"], json!(false));

    // A grep call naming a sensitive path is gate-denied before reaching mcp-grep.
    let denied = registry
        .call("grep", json!({ "pattern": "x", "path": ".env" }))
        .await;
    let err = denied.expect_err("sensitive path arg must be denied");
    assert!(
        err.to_string().contains("denied"),
        "denial must be gate-origin: {err}"
    );
}
