//! `mcp-bash <root>` — an MCP stdio server exposing a `bash` tool that runs the command in the
//! OS sandbox (always `SandboxPolicy::Os`, never None — fails closed without a backend). The
//! engine registers it as `bash` so the Ask-gate + sandbox-only registration apply unchanged.

use std::path::PathBuf;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let root = std::env::args().nth(1).map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("usage: mcp-bash <root>"))?;
    let _ = root; // server + serve land in Task 3
    Ok(())
}
