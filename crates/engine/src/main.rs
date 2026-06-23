//! `otto run "<goal>" [--root <path>]` — run a single turn and print the event stream.
//! `otto serve [--root <path>] [--port <p>] [--approve-edits] [--promote-loopback | --promote-vps <ws-endpoint> | --promote-microvm] [--accept-promotions]` — serve over WebSocket (needs OTTO_TOKEN).

use std::path::PathBuf;
use std::sync::Arc;

use otto_engine::{
    McpConnection, build_router, build_tool_registry, mcp_connect_bash, mcp_connect_fs,
    mcp_connect_git, mcp_connect_grep, resolve_tls_paths, run_goal, serve_app_with_base, serve_run,
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
                "usage:\n  otto run \"<goal>\" [--root <path>]\n  otto serve [--root <path>] [--port <p>] [--approve-edits] [--promote-loopback | --promote-vps <ws-endpoint> | --promote-microvm] [--accept-promotions]"
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

/// Read Firecracker microVM parameters from `OTTO_FC_*` env vars. Defaults match common Firecracker
/// quickstart values; required paths default to empty (validated later by the provisioner). Env
/// reading lives here at the CLI edge, never in `otto-remote`, mirroring how `build_router` reads env.
fn microvm_config_from_env() -> otto_engine::MicrovmConfig {
    let num = |k: &str, d: u32| {
        std::env::var(k)
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(d)
    };
    otto_engine::MicrovmConfig {
        kernel: PathBuf::from(std::env::var("OTTO_FC_KERNEL").unwrap_or_default()),
        rootfs: PathBuf::from(std::env::var("OTTO_FC_ROOTFS").unwrap_or_default()),
        fc_bin: PathBuf::from(
            std::env::var("OTTO_FC_BIN").unwrap_or_else(|_| "firecracker".to_string()),
        ),
        tap: std::env::var("OTTO_FC_TAP").unwrap_or_else(|_| "fc-tap0".to_string()),
        guest_ip: std::env::var("OTTO_FC_GUEST_IP").unwrap_or_else(|_| "172.16.0.2".to_string()),
        port: std::env::var("OTTO_FC_PORT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(7878),
        vcpus: num("OTTO_FC_VCPUS", 2),
        mem_mib: num("OTTO_FC_MEM_MIB", 1024),
        boot_timeout: std::time::Duration::from_secs(num("OTTO_FC_BOOT_TIMEOUT_SECS", 30) as u64),
    }
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
    approve_edits: bool,
) -> (ToolRegistry, Vec<McpConnection>) {
    let mut registry = if approve_edits {
        otto_engine::build_tool_registry_approving(tools_workspace, root.clone())
    } else {
        build_tool_registry(tools_workspace, root.clone())
    };
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
    let (tools, _mcp_conns) =
        build_tools_preferring_mcp(tools_workspace, root.clone(), false).await;
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
    let mut approve_edits = false;
    let mut promote_loopback = false;
    let mut accept_promotions = false;
    let mut promote_vps: Option<String> = None;
    let mut promote_microvm = false;
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
            "--approve-edits" => approve_edits = true,
            "--promote-loopback" => promote_loopback = true,
            "--accept-promotions" => accept_promotions = true,
            "--promote-vps" => match it.next() {
                Some(e) => promote_vps = Some(e.clone()),
                None => {
                    eprintln!("error: --promote-vps requires a ws://… endpoint");
                    std::process::exit(2);
                }
            },
            "--promote-microvm" => promote_microvm = true,
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
    let (tools, _mcp_conns) =
        build_tools_preferring_mcp(tools_workspace, root.clone(), approve_edits).await;
    let tools = Arc::new(tools);
    let store: Arc<dyn otto_persistence::SessionStore> =
        Arc::new(otto_persistence::SqliteStore::open(&open_db_path()).await?);
    let registry = Arc::new(otto_engine::build_default_registry());

    let service = otto_engine::EngineService::new(store, registry, router, orch_workspace, tools);
    let capabilities = otto_engine::build_capabilities();
    let promote = match (promote_loopback, promote_vps, promote_microvm) {
        (l, v, m) if (l as u8) + (v.is_some() as u8) + (m as u8) > 1 => {
            eprintln!(
                "error: --promote-loopback, --promote-vps, and --promote-microvm are mutually exclusive"
            );
            std::process::exit(2);
        }
        (true, _, _) => Some(otto_engine::PromoteConfig {
            token: token.clone(),
            // The dot-prefix is load-bearing: `LocalWorkspace::list` skips dot-directories, so a
            // provisioned engine's restored store/workspace under here is never recursively
            // captured by a later `workspace.snapshot()`. Do not rename without that guarantee.
            mode: otto_engine::PromoteMode::Loopback {
                base_dir: root.join(".otto-remotes"),
            },
        }),
        (_, Some(endpoint), _) => Some(otto_engine::PromoteConfig {
            token: token.clone(),
            mode: otto_engine::PromoteMode::Vps { endpoint },
        }),
        (_, _, true) => Some(otto_engine::PromoteConfig {
            token: token.clone(),
            mode: otto_engine::PromoteMode::Microvm {
                config: microvm_config_from_env(),
            },
        }),
        (false, None, false) => None,
    };
    // Resolve TLS: both flags -> wss; neither -> ws; one -> error (fail-closed). Resolved before
    // building the app so the scheme (and thus our own public ws base) is known up front.
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

    let scheme = if tls.is_some() { "wss" } else { "ws" };
    let public_ws_base = format!("{scheme}://127.0.0.1:{port}");
    let app = serve_app_with_base(
        service,
        token,
        capabilities,
        promote,
        accept_promotions,
        public_ws_base,
    );

    let addr = format!("127.0.0.1:{port}");
    let listener = std::net::TcpListener::bind(&addr)?;
    listener.set_nonblocking(true)?;
    eprintln!("otto serve listening on {scheme}://{addr}/ws");
    serve_run(listener, app, tls).await?;
    Ok(())
}
