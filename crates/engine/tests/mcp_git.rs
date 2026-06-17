//! End-to-end: build mcp-git, spawn it via the MCP client, register its tools in a gated
//! ToolRegistry, and confirm a git round-trip works and the gate denies a sensitive add. Loopback.

use std::sync::Arc;

use otto_engine::mcp_connect_git;
use otto_engine_core::tool::{DenyAsk, ToolRegistry};
use otto_tools::DefaultPermissionGate;
use serde_json::json;

async fn git(root: &std::path::Path, args: &[&str]) {
    let ok = tokio::process::Command::new("git").current_dir(root).args(args).status().await.unwrap().success();
    assert!(ok, "git {args:?} failed");
}

#[tokio::test]
async fn mcp_git_commits_and_stays_gated() {
    let bin = escargot::CargoBuild::new()
        .package("otto-mcp-git").bin("mcp-git").run().expect("build mcp-git").path().to_path_buf();

    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q"]).await;
    git(dir.path(), &["config", "user.name", "Test"]).await;
    git(dir.path(), &["config", "user.email", "test@example.com"]).await;
    git(dir.path(), &["config", "commit.gpgsign", "false"]).await;
    std::fs::write(dir.path().join("a.txt"), "hi\n").unwrap();

    let (_conn, tools) = mcp_connect_git(bin.to_str().unwrap(), dir.path()).await.expect("connect to mcp-git");
    let mut registry = ToolRegistry::new(Arc::new(DefaultPermissionGate::new()), Arc::new(DenyAsk));
    for t in tools { registry.register(t); }

    // add + commit a normal file via the MCP-backed tools.
    registry.call("git.add", json!({ "paths": ["a.txt"] })).await.unwrap();
    let out = registry.call("git.commit", json!({ "message": "via mcp" })).await.unwrap();
    assert!(out["hash"].as_str().unwrap().len() >= 7);

    // status reflects a clean tree after commit.
    let st = registry.call("git.status", json!({})).await.unwrap();
    assert!(st["changes"].as_array().unwrap().is_empty());

    // The gate denies staging a sensitive path before reaching mcp-git.
    std::fs::write(dir.path().join(".env"), "SECRET=x\n").unwrap();
    let denied = registry.call("git.add", json!({ "paths": [".env"] })).await;
    assert!(denied.expect_err("sensitive add must be denied").to_string().contains("denied"));
}
