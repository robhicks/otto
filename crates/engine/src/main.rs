//! `otto run "<goal>" [--root <path>]` — run a single turn and print the event stream.
//! `otto serve [--root <path>] [--port <p>]` — serve the engine over WebSocket (needs OTTO_TOKEN).

use std::path::PathBuf;
use std::sync::Arc;

use otto_engine::{
    McpConnection, build_router, build_tool_registry, mcp_connect_bash, mcp_connect_fs,
    mcp_connect_git, mcp_connect_grep, resolve_tls_paths, run_goal, serve_app, serve_run,
};
use otto_engine_core::tool::ToolRegistry;
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

fn mcp_fs_bin() -> String {
    std::env::var("OTTO_MCP_FS_BIN").unwrap_or_else(|_| "mcp-fs".to_string())
}

fn mcp_grep_bin() -> String {
    std::env::var("OTTO_MCP_GREP_BIN").unwrap_or_else(|_| "mcp-grep".to_string())
}

fn mcp_git_bin() -> String {
    std::env::var("OTTO_MCP_GIT_BIN").unwrap_or_else(|_| "mcp-git".to_string())
}

fn mcp_bash_bin() -> String {
    std::env::var("OTTO_MCP_BASH_BIN").unwrap_or_else(|_| "mcp-bash".to_string())
}

/// Build the tool registry, preferring mcp-fs for fs tools and falling back to in-process.
/// Also registers the grep tool from mcp-grep (additive; absent if mcp-grep can't be spawned).
/// Returns the registry and the live MCP connections to keep alive for the process lifetime.
async fn build_tools_preferring_mcp(
    tools_workspace: Arc<dyn Workspace>,
    root: PathBuf,
) -> (ToolRegistry, Vec<McpConnection>) {
    let mut registry = build_tool_registry(tools_workspace, root.clone());
    let mut conns = Vec::new();

    // fs: prefer mcp-fs, fall back to the in-process fs tools already in the registry.
    match mcp_connect_fs(&mcp_fs_bin(), &root).await {
        Ok((conn, mcp_tools)) => {
            for t in mcp_tools {
                registry.register(t);
            }
            conns.push(conn);
        }
        Err(e) => eprintln!("mcp-fs unavailable ({e}); using in-process fs tools"),
    }

    // grep: additive new capability — absent (logged) if mcp-grep can't be spawned.
    match mcp_connect_grep(&mcp_grep_bin(), &root).await {
        Ok((conn, mcp_tools)) => {
            for t in mcp_tools {
                registry.register(t);
            }
            conns.push(conn);
        }
        Err(e) => eprintln!("mcp-grep unavailable ({e}); search disabled"),
    }

    // git: additive — absent (logged) if mcp-git can't be spawned.
    match mcp_connect_git(&mcp_git_bin(), &root).await {
        Ok((conn, mcp_tools)) => {
            for t in mcp_tools {
                registry.register(t);
            }
            conns.push(conn);
        }
        Err(e) => eprintln!("mcp-git unavailable ({e}); git tools disabled"),
    }

    // bash: only when a sandbox backend exists (same rule build_tool_registry uses for the
    // in-process BashTool). Prefer mcp-bash, falling back to the in-process sandboxed BashTool
    // already in the registry. mcp-bash itself hardcodes Os, so it is always sandboxed.
    if otto_tools::os_sandbox_available() {
        match mcp_connect_bash(&mcp_bash_bin(), &root).await {
            Ok((conn, mcp_tools)) => {
                for t in mcp_tools {
                    registry.register(t);
                }
                conns.push(conn);
            }
            Err(e) => eprintln!("mcp-bash unavailable ({e}); using in-process sandboxed bash"),
        }
    }

    (registry, conns)
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
    let (tools, _mcp_conns) = build_tools_preferring_mcp(tools_workspace, root.clone()).await;
    // _mcp_conns is held until end of function so the mcp children stay alive.
    let tools = Arc::new(tools);
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
    let mut tls_cert: Option<PathBuf> = None;
    let mut tls_key: Option<PathBuf> = None;
    let mut it = positional.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--port" => match it.next().and_then(|s| s.parse().ok()) {
                Some(p) => port = p,
                None => {
                    eprintln!("error: --port requires a number");
                    std::process::exit(2);
                }
            },
            "--tls-cert" => match it.next() {
                Some(p) => tls_cert = Some(PathBuf::from(p)),
                None => {
                    eprintln!("error: --tls-cert requires a path");
                    std::process::exit(2);
                }
            },
            "--tls-key" => match it.next() {
                Some(p) => tls_key = Some(PathBuf::from(p)),
                None => {
                    eprintln!("error: --tls-key requires a path");
                    std::process::exit(2);
                }
            },
            _ => {}
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
    let (tools, _mcp_conns) = build_tools_preferring_mcp(tools_workspace, root.clone()).await;
    let tools = Arc::new(tools);
    let store: Arc<dyn otto_persistence::SessionStore> =
        Arc::new(otto_persistence::SqliteStore::open(&open_db_path()).await?);
    let registry = Arc::new(otto_engine::build_default_registry());

    let service = otto_engine::EngineService::new(store, registry, router, orch_workspace, tools);
    let app = serve_app(service, token);

    // Resolve TLS: both flags -> wss; neither -> ws; one -> error (fail-closed).
    let tls = match resolve_tls_paths(tls_cert, tls_key) {
        Ok(Some((cert, key))) => {
            // The rustls crypto provider is supplied at compile time by axum-server's tls-rustls (aws-lc-rs); no explicit install_default() is needed here.
            let cfg = axum_server::tls_rustls::RustlsConfig::from_pem_file(cert, key).await?;
            Some(cfg)
        }
        Ok(None) => None,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(2);
        }
    };

    let addr = format!("127.0.0.1:{port}");
    let listener = std::net::TcpListener::bind(&addr)?;
    listener.set_nonblocking(true)?;
    let scheme = if tls.is_some() { "wss" } else { "ws" };
    eprintln!("otto serve listening on {scheme}://{addr}/ws");
    serve_run(listener, app, tls).await?;
    Ok(())
}
