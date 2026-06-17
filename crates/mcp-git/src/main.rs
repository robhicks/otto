//! `mcp-git <root>` — an MCP stdio server performing git operations on the repo at <root> by
//! shelling out to `git`/`gh`. The engine spawns this and registers its tools behind the gate.

use std::path::PathBuf;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let root = std::env::args().nth(1).map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("usage: mcp-git <root>"))?;
    let _ = root; // core + serve land in later tasks
    Ok(())
}
