//! End-to-end: build mcp-fs, spawn it via the MCP client, register its tools in a gated
//! ToolRegistry, and confirm fs.read/write/list round-trip with the exact shapes the Coder depends
//! on, and that the gate denies a sensitive path BEFORE it reaches the server. Loopback (stdio).

use std::sync::Arc;

use otto_engine::mcp_connect_fs;
use otto_engine_core::tool::{DenyAsk, ToolRegistry};
use otto_tools::DefaultPermissionGate;
use serde_json::json;

#[tokio::test]
async fn mcp_fs_tools_round_trip_and_stay_gated() {
    // Build the mcp-fs binary once; get its path.
    let bin = escargot::CargoBuild::new()
        .package("otto-mcp-fs")
        .bin("mcp-fs")
        .run()
        .expect("build mcp-fs")
        .path()
        .to_path_buf();

    let dir = tempfile::tempdir().unwrap();

    // Connect and register the MCP fs tools in a gated registry.
    let (_conn, tools) = mcp_connect_fs(bin.to_str().unwrap(), dir.path())
        .await
        .expect("connect to mcp-fs");
    let mut registry = ToolRegistry::new(Arc::new(DefaultPermissionGate::new()), Arc::new(DenyAsk));
    for t in tools {
        registry.register(t);
    }

    // write -> {bytes_written}
    let w = registry
        .call("fs.write", json!({ "path": "a.txt", "contents": "hi" }))
        .await
        .unwrap();
    assert_eq!(w, json!({ "bytes_written": 2 }));

    // read -> {content} (exact shape the Coder relies on)
    let r = registry
        .call("fs.read", json!({ "path": "a.txt" }))
        .await
        .unwrap();
    assert_eq!(r, json!({ "content": "hi" }));

    // list -> {paths}
    let l = registry
        .call("fs.list", json!({ "glob": "**" }))
        .await
        .unwrap();
    assert!(l["paths"].as_array().unwrap().iter().any(|p| p == "a.txt"));

    // The gate denies a sensitive write BEFORE it reaches mcp-fs.
    let denied = registry
        .call(
            "fs.write",
            json!({ "path": ".env", "contents": "SECRET=x" }),
        )
        .await;
    let err = denied.expect_err("sensitive write must be denied");
    assert!(
        err.to_string().contains("denied"),
        "denial must come from the permission gate, got: {err}"
    );
    assert!(!dir.path().join(".env").exists());
}
