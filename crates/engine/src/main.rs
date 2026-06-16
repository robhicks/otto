//! `otto run "<goal>" [--root <path>]` — run a single turn and print the event stream.
//! `otto serve [--root <path>] [--port <p>]` — serve the engine over WebSocket (needs OTTO_TOKEN).

use std::path::PathBuf;
use std::sync::Arc;

use otto_engine::{build_router, build_tool_registry, run_goal, serve_app};
use otto_engine_core::traits::Workspace;
use otto_workspace::LocalWorkspace;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let command = args.next().unwrap_or_default();
    let rest: Vec<String> = args.collect();

    match command.as_str() {
        "run" => cmd_run(rest).await,
        "serve" => cmd_serve(rest).await,
        _ => {
            eprintln!(
                "usage:\n  otto run \"<goal>\" [--root <path>]\n  otto serve [--root <path>] [--port <p>]"
            );
            std::process::exit(2);
        }
    }
}

/// Parse `--root <path>` from args, defaulting to ".". Returns (root, remaining positional).
fn parse_root(args: &[String]) -> (PathBuf, Vec<String>) {
    let mut root = PathBuf::from(".");
    let mut positional = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--root" {
            if let Some(p) = it.next() {
                root = PathBuf::from(p);
            } else {
                eprintln!("error: --root requires a path");
                std::process::exit(2);
            }
        } else {
            positional.push(a.clone());
        }
    }
    (root, positional)
}

fn open_db_path() -> String {
    std::env::var("OTTO_DB").unwrap_or_else(|_| "otto-sessions.db".to_string())
}

async fn cmd_run(args: Vec<String>) -> anyhow::Result<()> {
    let (root, positional) = parse_root(&args);
    let goal = positional.into_iter().next().unwrap_or_else(|| {
        eprintln!("error: missing goal");
        std::process::exit(2);
    });

    let router: Arc<dyn otto_engine_core::Router> = Arc::from(build_router());
    let orch_workspace: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(root.clone()));
    let tools_workspace: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(root.clone()));
    let tools = Arc::new(build_tool_registry(tools_workspace, root.clone()));
    let store: Arc<dyn otto_persistence::SessionStore> =
        Arc::new(otto_persistence::SqliteStore::open(&open_db_path()).await?);

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

async fn cmd_serve(args: Vec<String>) -> anyhow::Result<()> {
    let (root, positional) = parse_root(&args);
    let mut port: u16 = std::env::var("OTTO_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(7878);
    let mut it = positional.iter();
    while let Some(a) = it.next() {
        if a == "--port" {
            match it.next().and_then(|s| s.parse().ok()) {
                Some(p) => port = p,
                None => {
                    eprintln!("error: --port requires a number");
                    std::process::exit(2);
                }
            }
        }
    }

    // Auth is mandatory and fail-closed: refuse to start without a token.
    let token = match std::env::var("OTTO_TOKEN") {
        Ok(t) if !t.is_empty() => t,
        _ => {
            eprintln!("error: OTTO_TOKEN must be set to run `otto serve`");
            std::process::exit(2);
        }
    };

    let router: Arc<dyn otto_engine_core::Router> = Arc::from(build_router());
    let orch_workspace: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(root.clone()));
    let tools_workspace: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(root.clone()));
    let tools = Arc::new(build_tool_registry(tools_workspace, root.clone()));
    let store: Arc<dyn otto_persistence::SessionStore> =
        Arc::new(otto_persistence::SqliteStore::open(&open_db_path()).await?);
    let registry = Arc::new(otto_engine::build_default_registry());

    let service = otto_engine::EngineService::new(store, registry, router, orch_workspace, tools);
    let app = serve_app(service, token);

    let addr = format!("127.0.0.1:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    eprintln!("otto serve listening on ws://{addr}/ws");
    axum::serve(listener, app).await?;
    Ok(())
}
