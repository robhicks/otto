//! `otto run "<goal>" [--root <path>]` — run a single turn and print the event stream.

use std::path::PathBuf;
use std::sync::Arc;

use otto_engine::{build_router, build_tool_registry, run_goal};
use otto_engine_core::traits::Workspace;
use otto_workspace::LocalWorkspace;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let command = args.next().unwrap_or_default();
    if command != "run" {
        eprintln!("usage: otto run \"<goal>\" [--root <path>]");
        std::process::exit(2);
    }

    let goal = args.next().unwrap_or_else(|| {
        eprintln!("error: missing goal");
        std::process::exit(2);
    });

    let mut root = PathBuf::from(".");
    if let Some(flag) = args.next() {
        if flag == "--root" {
            match args.next() {
                Some(path) => root = PathBuf::from(path),
                None => {
                    eprintln!("error: --root requires a path");
                    std::process::exit(2);
                }
            }
        }
    }

    let router: Arc<dyn otto_engine_core::Router> = Arc::from(build_router());
    let orch_workspace: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(root.clone()));
    let tools_workspace: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(root.clone()));
    let tools = Arc::new(build_tool_registry(tools_workspace, root.clone()));

    // The session store. Defaults to `otto-sessions.db` in the current dir; override with
    // OTTO_DB. Sessions and their event logs accumulate here across runs.
    let db_path = std::env::var("OTTO_DB").unwrap_or_else(|_| "otto-sessions.db".to_string());
    let store: Arc<dyn otto_persistence::SessionStore> =
        Arc::new(otto_persistence::SqliteStore::open(&db_path).await?);

    let (events, outcome) = run_goal(&goal, store, router, orch_workspace, tools).await?;
    for event in &events {
        println!("[{:>3}] {:?}", event.seq, event.kind);
    }
    println!("turn ok = {}", outcome.ok);
    if !outcome.ok {
        std::process::exit(1);
    }
    Ok(())
}
