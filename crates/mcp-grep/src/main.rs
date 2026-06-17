//! `mcp-grep <root>` — an MCP stdio server providing ripgrep-style regex search over <root>,
//! path-contained and never searching sensitive files. The engine spawns this and registers the
//! `grep` tool behind the gate.

use std::path::PathBuf;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let root = std::env::args().nth(1).map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("usage: mcp-grep <root>"))?;
    let _ = root; // search + serve land in Tasks 2–3
    Ok(())
}
