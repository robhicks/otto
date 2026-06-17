//! `mcp-fs <root>` — an MCP stdio server exposing path-contained fs.read/fs.write/fs.list over a
//! `LocalWorkspace` rooted at <root>. The engine spawns this and registers its tools behind the gate.

use std::path::PathBuf;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let root = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("usage: mcp-fs <root>"))?;
    let _ = root; // tools land in Task 2
    // Serve over stdio. The exact rmcp serve call is filled in in Task 2.
    Ok(())
}
