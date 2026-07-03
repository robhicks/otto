//! `otto run "<goal>" [--root <path>] [--agent <name>]` — run a single turn (or a named custom agent) and print output.
//! `otto serve [--root <path>] [--port <p>] [--approve-edits] [--promote-loopback | --promote-vps <ws-endpoint> | --promote-microvm] [--accept-promotions]` — serve over WebSocket (needs OTTO_TOKEN).

use std::path::PathBuf;
use std::sync::Arc;

use otto_engine::{
    McpConnection, build_router, build_tool_registry, mcp_connect_bash, mcp_connect_fs,
    mcp_connect_git, mcp_connect_grep, mcp_connect_plugin_server, resolve_tls_paths, run_goal,
    serve_app_with_base, serve_run,
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
                "usage:\n  otto run \"<goal>\" [--root <path>] [--agent <name>]\n  otto serve [--root <path>] [--port <p>] [--approve-edits] [--promote-loopback | --promote-vps <ws-endpoint> | --promote-microvm] [--accept-promotions]"
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

/// Parse `--agent <name>` from args. Returns (Some(name), remaining) or (None, args).
fn parse_agent_flag(args: &[String]) -> (Option<String>, Vec<String>) {
    let mut name = None;
    let mut rest = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--agent" {
            match it.next() {
                Some(v) => name = Some(v.clone()),
                None => {
                    eprintln!("error: --agent requires a name");
                    std::process::exit(2);
                }
            }
        } else {
            rest.push(a.clone());
        }
    }
    (name, rest)
}

/// Parse `--command <name>` from args. Returns (Some(name), remaining) or (None, args). The
/// remaining args are the command's positional arguments ($1.., $ARGUMENTS).
fn parse_command_flag(args: &[String]) -> (Option<String>, Vec<String>) {
    let mut name = None;
    let mut rest = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--command" {
            match it.next() {
                Some(v) => name = Some(v.clone()),
                None => {
                    eprintln!("error: --command requires a name");
                    std::process::exit(2);
                }
            }
        } else {
            rest.push(a.clone());
        }
    }
    (name, rest)
}

/// The user-global `.claude/` base: the OS home directory (empty path if it cannot be
/// determined → user-global discovery is simply skipped). Uses `dirs::home_dir` so it
/// works when `$HOME` is unset (Unix `getpwuid` fallback), matching the rest of the crate.
fn home_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_default()
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
    permissions: &otto_extensions::PermissionRules,
) -> (ToolRegistry, Vec<McpConnection>) {
    let mut registry = if !permissions.is_empty() {
        // Permission rules override the default gate with a PolicyGate, composed with approval
        // mode when the caller requests it (e.g. `otto serve --approve-edits`).
        otto_engine::build_tool_registry_with_permissions(
            tools_workspace,
            root.clone(),
            permissions,
            approve_edits,
        )
    } else if approve_edits {
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

/// The tool-registry composition `otto serve` uses: the permission/approval gate from
/// `build_tools_preferring_mcp`, then hook-wrapping on top via `register_hooks` — the same two
/// steps `cmd_run` performs inline. Skills and plugin MCP servers are NOT registered here; that
/// remains deferred for the serve path.
async fn build_serve_tools(
    ext: &otto_extensions::Extensions,
    tools_workspace: Arc<dyn Workspace>,
    root: PathBuf,
    approve_edits: bool,
) -> (ToolRegistry, Vec<McpConnection>) {
    let (mut tools, conns) = build_tools_preferring_mcp(
        tools_workspace,
        root.clone(),
        approve_edits,
        &ext.permissions,
    )
    .await;
    register_hooks(&mut tools, &ext.hooks, &root);
    (tools, conns)
}

/// Register the built-in `skill` tool when any skills were discovered. No-op otherwise, so a
/// workspace with no `.claude/skills/` leaves the spine's tool set byte-for-byte unchanged.
fn register_skills(
    registry: &mut otto_engine_core::tool::ToolRegistry,
    skills: &[otto_extensions::CustomSkillDef],
) {
    if !skills.is_empty() {
        registry.register(Arc::new(otto_extensions::SkillTool::new(skills)));
    }
}

/// Wrap every registered tool with hook decorators. With no hooks configured, nothing happens.
/// If hooks ARE configured but no OS sandbox backend is available, the hooks are skipped (their
/// commands are never run unsandboxed) and a loud warning is printed: a configured blocking
/// `PreToolUse` hook will NOT fire, so it cannot protect the call — unlike `bash`, the guarded
/// tools still run. Only when hooks exist AND a sandbox backend is present is every tool wrapped.
fn register_hooks(
    registry: &mut otto_engine_core::tool::ToolRegistry,
    hooks: &otto_extensions::HookSet,
    root: &std::path::Path,
) {
    if hooks.is_empty() {
        return;
    }
    if !otto_tools::os_sandbox_available() {
        eprintln!(
            "warning: {} hook(s) are configured in settings.json but no OS sandbox backend \
             (bwrap/sandbox-exec) is available — hooks will NOT run, so tool calls are NOT \
             guarded by them. Install bwrap (Linux) or sandbox-exec (macOS) to enable hooks.",
            hooks.pre_tool_use.len() + hooks.post_tool_use.len()
        );
        return;
    }
    let exec: Arc<dyn otto_extensions::HookExecutor> =
        Arc::new(otto_engine::SandboxedHookExecutor::new(root.to_path_buf()));
    registry.wrap_each(|t| otto_extensions::HookedTool::wrap(t, hooks, exec.clone()));
}

async fn cmd_run(args: Vec<String>) -> anyhow::Result<()> {
    let (root, after_root) = parse_root(&args);
    let (command_name, after_cmd) = parse_command_flag(&after_root);
    let (agent_name, positional) = parse_agent_flag(&after_cmd);

    if command_name.is_some() && agent_name.is_some() {
        eprintln!("error: --command and --agent are mutually exclusive");
        std::process::exit(2);
    }

    if let Some(cmd) = command_name {
        return run_command_in(&cmd, &positional, root, home_dir()).await;
    }

    let goal = positional.into_iter().next().unwrap_or_else(|| {
        eprintln!("error: missing goal");
        std::process::exit(2);
    });

    if let Some(name) = agent_name {
        return run_custom_agent(&name, &goal, root).await;
    }

    let router: Arc<dyn otto_engine_core::Router> = Arc::from(build_router());
    let orch_workspace: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(root.clone()));
    let tools_workspace: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(root.clone()));
    // Discover extensions first: the permission rules are needed at registry-construction time so
    // the gate can be a PolicyGate.
    let ext = otto_extensions::discover(&root, &home_dir());
    let (mut tools, mut mcp_conns) =
        build_tools_preferring_mcp(tools_workspace, root.clone(), false, &ext.permissions).await;
    // mcp_conns is held until end of function so the mcp children stay alive.
    // Register discovered skills as the gated `skill` tool so spine agents can load them mid-turn.
    register_skills(&mut tools, &ext.skills);
    register_hooks(&mut tools, &ext.hooks, &root);
    // Bundled plugin MCP servers (Plan B): spawn each enabled plugin's servers through the same
    // stdio connect path otto uses for its own MCP servers; register the namespaced tools behind the
    // gate. A server that won't spawn is logged and skipped (additive, never fatal). With no
    // `.claude/plugins/`, `ext.mcp_servers` is empty and the tool set is byte-for-byte unchanged.
    // These register after `register_hooks`' `wrap_each`, so plugin MCP tools are gate-guarded but
    // not hook-wrapped this slice — a `plugin__…` hook matcher would not fire (deferred extension).
    for spec in &ext.mcp_servers {
        match mcp_connect_plugin_server(spec).await {
            Ok((conn, mcp_tools)) => {
                for t in mcp_tools {
                    tools.register(t);
                }
                mcp_conns.push(conn);
            }
            Err(e) => eprintln!(
                "plugin mcp server {}:{} unavailable ({e}); skipping",
                spec.namespace, spec.server_key
            ),
        }
    }
    let tools = Arc::new(tools);
    let store: Arc<dyn otto_persistence::SessionStore> =
        Arc::new(otto_persistence::SqliteStore::open(&open_db_path()).await?);

    let retriever = otto_engine::build_retriever(&root).await;
    let (events, outcome) =
        run_goal(&goal, store, router, orch_workspace, tools, retriever).await?;
    for event in &events {
        println!("[{:>3}] {:?}", event.seq, event.kind);
    }
    println!("turn ok = {}", outcome.ok);
    if !outcome.ok {
        std::process::exit(1);
    }
    Ok(())
}

/// Run a discovered custom agent through the `TaskTool` dispatch path and print its output.
/// Unknown agent name (or no `.claude/agents/`) is a clear error.
async fn run_custom_agent(name: &str, goal: &str, root: PathBuf) -> anyhow::Result<()> {
    run_custom_agent_in(name, goal, root, home_dir()).await
}

/// `run_custom_agent` with the home directory injected (so tests can supply an empty home and
/// never read the developer's real `~/.claude`). All dispatch logic lives here.
async fn run_custom_agent_in(
    name: &str,
    goal: &str,
    root: PathBuf,
    home: PathBuf,
) -> anyhow::Result<()> {
    use otto_engine_core::AgentRegistry;
    use otto_engine_core::WorkspaceRead;
    use otto_engine_core::tool::Tool;
    use otto_extensions::{MarkdownAgent, TaskTool};
    use otto_protocol::Role;
    use std::collections::HashMap;

    let ext = otto_extensions::discover(&root, &home);
    if !ext.hooks.is_empty() {
        eprintln!(
            "warning: settings.json hooks are configured but are NOT enforced on this path \
             (hooks are wired only on the `otto run` spine for now)."
        );
    }
    if !ext.permissions.is_empty() {
        eprintln!(
            "warning: settings.json permissions are configured but are NOT enforced on \
             this path (permissions are wired only on the `otto run` spine for now)."
        );
    }

    let mut registry = AgentRegistry::new();
    let mut allowlists: HashMap<String, Option<Vec<String>>> = HashMap::new();
    // Pin the remote model to the top-level `--agent`'s `model:` field. Nested Task
    // sub-dispatches inherit this same pinned router (per-sub-agent model is deferred).
    let mut model_override: Option<String> = None;
    for def in ext.agents {
        if def.name == name {
            model_override = def.model.clone();
        }
        allowlists.insert(def.name.clone(), def.tools.clone());
        registry.register(
            Role::Custom(def.name.clone()),
            Arc::new(MarkdownAgent::new(def)),
        );
    }

    if registry.get(&Role::Custom(name.to_string())).is_err() {
        anyhow::bail!(
            "no custom agent named '{name}' in ~/.claude/agents/ or {}/.claude/agents/",
            root.display()
        );
    }

    let router: Arc<dyn otto_engine_core::Router> = Arc::from(
        otto_engine::build_router_with_model(model_override.as_deref()),
    );
    let read_ws: Arc<dyn WorkspaceRead> = Arc::new(LocalWorkspace::new(root.clone()));
    let tools_ws: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(root.clone()));
    // NOTE: hooks/skills/plugin MCP servers are wired only in the main `otto run` spine for now; the
    // --agent/--command/serve paths are deferred (extensions hooks slice).
    let (base_tools, _mcp) = build_tools_preferring_mcp(
        tools_ws,
        root,
        false,
        &otto_extensions::PermissionRules::default(),
    )
    .await;

    let task = TaskTool::new(
        router,
        read_ws,
        Arc::new(registry),
        Arc::new(base_tools),
        allowlists,
    );
    let out = task
        .call(serde_json::json!({ "agent": name, "prompt": goal }))
        .await?;
    let text = out
        .get("text")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("task dispatch returned no `text` field"))?;
    println!("{text}");
    Ok(())
}

/// Apply a command's `allowed_tools` to its tool registry, matching the agent narrowing
/// convention: an absent allowlist (`None`) keeps every tool; a present allowlist narrows to
/// exactly that intersection (a present-but-empty list yields no tools). `subset` shares the
/// underlying gate, so the inviolable sensitive-path floor is preserved within the narrowed set,
/// and narrowing can only remove tools — never widen access.
fn narrow_for_command(
    tools: otto_engine_core::tool::ToolRegistry,
    allowed: &Option<Vec<String>>,
) -> otto_engine_core::tool::ToolRegistry {
    match allowed {
        Some(list) => tools.subset(list),
        None => tools,
    }
}

/// Expand a discovered command (`expand_args` then gated `!bash`/`@file` injection) and run the
/// result as the goal of a normal spine turn. `home` is injected so tests stay hermetic.
async fn run_command_in(
    name: &str,
    args: &[String],
    root: PathBuf,
    home: PathBuf,
) -> anyhow::Result<()> {
    use otto_extensions::{expand_args, resolve_injections};

    let ext = otto_extensions::discover(&root, &home);
    if !ext.hooks.is_empty() {
        eprintln!(
            "warning: settings.json hooks are configured but are NOT enforced on this path \
             (hooks are wired only on the `otto run` spine for now)."
        );
    }
    if !ext.permissions.is_empty() {
        eprintln!(
            "warning: settings.json permissions are configured but are NOT enforced on \
             this path (permissions are wired only on the `otto run` spine for now)."
        );
    }
    let def = ext
        .commands
        .into_iter()
        .find(|c| c.name == name)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no command named '{name}' in ~/.claude/commands/ or {}/.claude/commands/",
                root.display()
            )
        })?;

    // The gated tool registry: injection reaches fs.read/bash through the same gate the spine
    // turn uses (bash only when a sandbox backend exists). Reused as the turn's tools.
    // NOTE: hooks/skills/plugin MCP servers are wired only in the main `otto run` spine for now; the
    // --agent/--command/serve paths are deferred (extensions hooks slice).
    let tools_workspace: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(root.clone()));
    let (tools, _mcp_conns) = build_tools_preferring_mcp(
        tools_workspace,
        root.clone(),
        false,
        &otto_extensions::PermissionRules::default(),
    )
    .await;
    // _mcp_conns is held until end of function so the mcp children stay alive.
    // Narrow the registry to the command's allowed-tools (None = all tools) before it is used for
    // BOTH injection resolution and the spine turn — so a disallowed tool is fail-closed.
    let tools = Arc::new(narrow_for_command(tools, &def.allowed_tools));

    // Args are substituted first, then the whole string is scanned for `!`cmd`/@path`
    // injections — so a user-supplied arg is intentionally re-scanned (Claude-Code parity).
    // Its capability ceiling is exactly the gate's: the sandbox + sensitive-path floor still
    // apply, and any denied/failed injection aborts here (fail-closed) before the spine turn.
    let expanded = expand_args(&def.template, args);
    let goal = resolve_injections(&expanded, tools.as_ref()).await?;

    // Pin the remote model to the command's `model:` field (None = normal env-based routing).
    let router: Arc<dyn otto_engine_core::Router> =
        Arc::from(otto_engine::build_router_with_model(def.model.as_deref()));
    let orch_workspace: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(root.clone()));
    let store: Arc<dyn otto_persistence::SessionStore> =
        Arc::new(otto_persistence::SqliteStore::open(&open_db_path()).await?);
    let retriever = otto_engine::build_retriever(&root).await;

    let (events, outcome) =
        run_goal(&goal, store, router, orch_workspace, tools, retriever).await?;
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

    let ext = otto_extensions::discover(&root, &home_dir());
    if !ext.hooks.is_empty() {
        eprintln!(
            "warning: settings.json hooks are configured but are NOT enforced on the serve \
             path (hooks are wired only on the `otto run` spine for now)."
        );
    }

    let router: Arc<dyn otto_engine_core::Router> = Arc::from(build_router());
    let orch_workspace: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(root.clone()));
    let tools_workspace: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(root.clone()));
    // NOTE: hooks/skills/plugin MCP servers are wired only in the main `otto run` spine for now;
    // the --agent/--command paths are deferred (extensions hooks slice). Permissions ARE
    // enforced here, composed with --approve-edits when both are configured (see
    // build_tool_registry_inner).
    let (tools, _mcp_conns) = build_tools_preferring_mcp(
        tools_workspace,
        root.clone(),
        approve_edits,
        &ext.permissions,
    )
    .await;
    let tools = Arc::new(tools);
    let store: Arc<dyn otto_persistence::SessionStore> =
        Arc::new(otto_persistence::SqliteStore::open(&open_db_path()).await?);
    let registry = Arc::new(otto_engine::build_default_registry());

    let retriever = otto_engine::build_retriever(&root).await;
    let service = otto_engine::EngineService::new(store, registry, router, orch_workspace, tools)
        .with_retriever(retriever);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_agent_flag_extracts_name() {
        let args = vec![
            "--agent".to_string(),
            "reviewer".to_string(),
            "do it".to_string(),
        ];
        let (name, rest) = parse_agent_flag(&args);
        assert_eq!(name, Some("reviewer".to_string()));
        assert_eq!(rest, vec!["do it".to_string()]);
    }

    #[test]
    fn parse_agent_flag_absent_is_none() {
        let args = vec!["do it".to_string()];
        let (name, rest) = parse_agent_flag(&args);
        assert_eq!(name, None);
        assert_eq!(rest, vec!["do it".to_string()]);
    }

    #[tokio::test]
    async fn run_custom_agent_dispatches_and_errors_on_unknown() {
        use std::fs;
        let proj = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap(); // empty → no user-global agents
        let agents = proj.path().join(".claude").join("agents");
        fs::create_dir_all(&agents).unwrap();
        fs::write(
            agents.join("echoer.md"),
            "---\nname: echoer\ndescription: echoes\ntools: fs.read\n---\nYou are an echo agent.\n",
        )
        .unwrap();

        // Known agent dispatches successfully (offline LocalProvider, deterministic).
        let ok = run_custom_agent_in(
            "echoer",
            "hello",
            proj.path().to_path_buf(),
            home.path().to_path_buf(),
        )
        .await;
        assert!(ok.is_ok(), "expected dispatch to succeed: {ok:?}");

        // Unknown agent name errors.
        let err = run_custom_agent_in(
            "nope",
            "hello",
            proj.path().to_path_buf(),
            home.path().to_path_buf(),
        )
        .await;
        assert!(err.is_err());
        assert!(
            err.unwrap_err()
                .to_string()
                .contains("no custom agent named")
        );
    }

    #[tokio::test]
    async fn run_custom_agent_with_model_field_runs_offline() {
        use std::fs;
        // A custom agent declaring `model:` still runs a deterministic offline dispatch when no
        // ANTHROPIC_API_KEY is set (graceful fallback).
        let proj = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap(); // empty → no user-global agents
        let agents = proj.path().join(".claude").join("agents");
        fs::create_dir_all(&agents).unwrap();
        fs::write(
            agents.join("reviewer.md"),
            "---\nname: reviewer\ndescription: Reviews code\nmodel: claude-opus-4-8\n---\nYou review code.\n",
        )
        .unwrap();

        let ok = run_custom_agent_in(
            "reviewer",
            "look at lib.rs",
            proj.path().to_path_buf(),
            home.path().to_path_buf(),
        )
        .await;
        assert!(
            ok.is_ok(),
            "expected model-pinned agent to run offline: {ok:?}"
        );
    }

    #[test]
    fn parse_command_flag_extracts_name_and_keeps_args() {
        let args = vec![
            "--command".to_string(),
            "git:commit".to_string(),
            "fix".to_string(),
            "parser".to_string(),
        ];
        let (name, rest) = parse_command_flag(&args);
        assert_eq!(name, Some("git:commit".to_string()));
        assert_eq!(rest, vec!["fix".to_string(), "parser".to_string()]);
    }

    #[test]
    fn parse_command_flag_absent_is_none() {
        let args = vec!["just a goal".to_string()];
        let (name, rest) = parse_command_flag(&args);
        assert_eq!(name, None);
        assert_eq!(rest, vec!["just a goal".to_string()]);
    }

    #[tokio::test]
    async fn run_command_expands_and_runs_spine() {
        use std::fs;
        let proj = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap(); // empty → no user-global commands
        let cmds = proj.path().join(".claude").join("commands").join("greet");
        fs::create_dir_all(&cmds).unwrap();
        fs::write(cmds.join("hello.md"), "Say hello to $1.\n").unwrap();

        // Known command expands ($1 → "world") and runs an offline, deterministic spine turn.
        let ok = run_command_in(
            "greet:hello",
            &["world".to_string()],
            proj.path().to_path_buf(),
            home.path().to_path_buf(),
        )
        .await;
        assert!(ok.is_ok(), "expected command run to succeed: {ok:?}");

        // Unknown command name errors.
        let err = run_command_in(
            "nope",
            &[],
            proj.path().to_path_buf(),
            home.path().to_path_buf(),
        )
        .await;
        assert!(err.is_err());
        assert!(err.unwrap_err().to_string().contains("no command named"));
    }

    #[tokio::test]
    async fn run_command_with_model_field_runs_offline() {
        use std::fs;
        // A command declaring `model:` still runs a deterministic offline turn when no
        // ANTHROPIC_API_KEY is set (graceful fallback + stderr warning).
        let proj = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap(); // empty → no user-global commands
        let cmds = proj.path().join(".claude").join("commands");
        fs::create_dir_all(&cmds).unwrap();
        fs::write(
            cmds.join("plan.md"),
            "---\nmodel: claude-opus-4-8\n---\nDescribe the plan for $1.\n",
        )
        .unwrap();

        let ok = run_command_in(
            "plan",
            &["auth".to_string()],
            proj.path().to_path_buf(),
            home.path().to_path_buf(),
        )
        .await;
        assert!(
            ok.is_ok(),
            "expected model-pinned command to run offline: {ok:?}"
        );
    }

    #[test]
    fn register_skills_adds_skill_tool_when_present() {
        use std::fs;
        let proj = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap(); // empty → no user-global skills
        let skill = proj.path().join(".claude").join("skills").join("greeter");
        fs::create_dir_all(&skill).unwrap();
        fs::write(
            skill.join("SKILL.md"),
            "---\nname: greeter\ndescription: greets\n---\nSay hi.\n",
        )
        .unwrap();

        let ext = otto_extensions::discover(proj.path(), home.path());
        let mut reg = otto_engine::build_tool_registry(
            Arc::new(LocalWorkspace::new(proj.path().to_path_buf())),
            proj.path().to_path_buf(),
        );
        register_skills(&mut reg, &ext.skills);
        assert!(reg.tool_names().iter().any(|n| n == "skill"));
    }

    #[test]
    fn register_skills_is_noop_when_absent() {
        let proj = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let ext = otto_extensions::discover(proj.path(), home.path());
        let mut reg = otto_engine::build_tool_registry(
            Arc::new(LocalWorkspace::new(proj.path().to_path_buf())),
            proj.path().to_path_buf(),
        );
        register_skills(&mut reg, &ext.skills);
        assert!(!reg.tool_names().iter().any(|n| n == "skill"));
    }

    #[tokio::test]
    async fn discovered_pretooluse_hook_blocks_a_tool_call() {
        use otto_engine_core::tool::ToolRegistry;
        if !otto_tools::os_sandbox_available() {
            eprintln!("skipping hooks blocking test: no OS sandbox backend");
            return;
        }
        let proj = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        std::fs::write(proj.path().join("target.txt"), "hi").unwrap();
        let claude = proj.path().join(".claude");
        std::fs::create_dir_all(&claude).unwrap();
        std::fs::write(
            claude.join("settings.json"),
            r#"{"hooks":{"PreToolUse":[{"matcher":"fs.read","hooks":[{"type":"command","command":"exit 2"}]}]}}"#,
        )
        .unwrap();

        let ws: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(proj.path().to_path_buf()));
        let mut tools: ToolRegistry =
            otto_engine::build_tool_registry(ws, proj.path().to_path_buf());
        let ext = otto_extensions::discover(proj.path(), home.path());
        super::register_hooks(&mut tools, &ext.hooks, proj.path());

        let err = tools
            .call("fs.read", serde_json::json!({ "path": "target.txt" }))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("blocked by PreToolUse hook"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn enabled_plugin_pretooluse_hook_blocks_a_tool_call() {
        use otto_engine_core::tool::ToolRegistry;
        if !otto_tools::os_sandbox_available() {
            eprintln!("skipping plugin hook blocking test: no OS sandbox backend");
            return;
        }
        let proj = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        std::fs::write(proj.path().join("target.txt"), "hi").unwrap();

        // A marketplace under the project base offering one local plugin whose bundled hooks.json
        // blocks fs.read; enabled via project settings.json.
        let mp_dir = proj
            .path()
            .join(".claude")
            .join("plugins")
            .join("marketplaces")
            .join("acme");
        let cp = mp_dir.join(".claude-plugin");
        std::fs::create_dir_all(&cp).unwrap();
        std::fs::write(
            cp.join("marketplace.json"),
            r#"{"name":"acme","plugins":[{"name":"guard","source":"./plugins/guard"}]}"#,
        )
        .unwrap();
        let proot = mp_dir.join("plugins").join("guard");
        let pcp = proot.join(".claude-plugin");
        std::fs::create_dir_all(&pcp).unwrap();
        std::fs::write(pcp.join("plugin.json"), r#"{"name":"guard"}"#).unwrap();
        let hooks_dir = proot.join("hooks");
        std::fs::create_dir_all(&hooks_dir).unwrap();
        std::fs::write(
            hooks_dir.join("hooks.json"),
            r#"{"hooks":{"PreToolUse":[{"matcher":"fs.read","hooks":[{"type":"command","command":"exit 2"}]}]}}"#,
        )
        .unwrap();
        std::fs::write(
            proj.path().join(".claude").join("settings.json"),
            r#"{"enabledPlugins":{"guard@acme":true}}"#,
        )
        .unwrap();

        let ws: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(proj.path().to_path_buf()));
        let mut tools: ToolRegistry =
            otto_engine::build_tool_registry(ws, proj.path().to_path_buf());
        let ext = otto_extensions::discover(proj.path(), home.path());
        super::register_hooks(&mut tools, &ext.hooks, proj.path());

        let err = tools
            .call("fs.read", serde_json::json!({ "path": "target.txt" }))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("blocked by PreToolUse hook"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn discovered_posttooluse_hook_runs_without_blocking() {
        use otto_engine_core::tool::ToolRegistry;
        if !otto_tools::os_sandbox_available() {
            eprintln!("skipping hooks PostToolUse test: no OS sandbox backend");
            return;
        }
        let proj = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        std::fs::write(proj.path().join("target.txt"), "hi").unwrap();
        let claude = proj.path().join(".claude");
        std::fs::create_dir_all(&claude).unwrap();
        // PostToolUse hook on fs.read drops a marker file in the workspace root (sandbox allows it).
        std::fs::write(
            claude.join("settings.json"),
            r#"{"hooks":{"PostToolUse":[{"matcher":"fs.read","hooks":[{"type":"command","command":"touch post_ran.marker"}]}]}}"#,
        )
        .unwrap();

        let ws: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(proj.path().to_path_buf()));
        let mut tools: ToolRegistry =
            otto_engine::build_tool_registry(ws, proj.path().to_path_buf());
        let ext = otto_extensions::discover(proj.path(), home.path());
        super::register_hooks(&mut tools, &ext.hooks, proj.path());

        // PostToolUse must not block: the call returns the file content normally.
        let out = tools
            .call("fs.read", serde_json::json!({ "path": "target.txt" }))
            .await
            .unwrap();
        assert!(
            out.to_string().contains("hi"),
            "expected fs.read to return content, got: {out}"
        );
        // And the PostToolUse hook actually fired (marker created in the workspace root).
        assert!(
            proj.path().join("post_ran.marker").exists(),
            "PostToolUse hook did not run"
        );
    }

    #[tokio::test]
    async fn no_settings_leaves_tools_unwrapped() {
        use otto_engine_core::tool::ToolRegistry;
        let proj = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        std::fs::write(proj.path().join("target.txt"), "hi").unwrap();

        let ws: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(proj.path().to_path_buf()));
        let mut tools: ToolRegistry =
            otto_engine::build_tool_registry(ws, proj.path().to_path_buf());
        let ext = otto_extensions::discover(proj.path(), home.path());
        super::register_hooks(&mut tools, &ext.hooks, proj.path());

        let out = tools
            .call("fs.read", serde_json::json!({ "path": "target.txt" }))
            .await
            .unwrap();
        assert!(
            out.to_string().contains("hi"),
            "expected fs.read to return file content, got: {out}"
        );
    }

    #[test]
    fn narrow_for_command_applies_the_allowlist_convention() {
        use otto_workspace::LocalWorkspace;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let build = || {
            otto_engine::build_tool_registry(
                Arc::new(LocalWorkspace::new(root.clone())),
                root.clone(),
            )
        };

        // None → all tools unchanged (fs.write is always registered).
        let all: std::collections::BTreeSet<String> = build().tool_names().into_iter().collect();
        let kept: std::collections::BTreeSet<String> = narrow_for_command(build(), &None)
            .tool_names()
            .into_iter()
            .collect();
        assert_eq!(kept, all, "None must keep every base tool");

        // Some(list) → narrowed to exactly the intersection.
        let only_read =
            narrow_for_command(build(), &Some(vec!["fs.read".to_string()])).tool_names();
        assert_eq!(only_read, vec!["fs.read".to_string()]);

        // Some([]) → no tools.
        assert!(
            narrow_for_command(build(), &Some(vec![]))
                .tool_names()
                .is_empty(),
            "an empty allowlist must yield no tools"
        );

        // An unknown name is silently dropped (intersection), never an error.
        let unknown =
            narrow_for_command(build(), &Some(vec!["does.not.exist".to_string()])).tool_names();
        assert!(unknown.is_empty(), "unknown tool names are dropped");
    }

    #[tokio::test]
    async fn command_allowed_tools_narrows_and_blocks_disallowed_injection() {
        use std::fs;
        let proj = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap(); // empty → no user-global commands
        fs::write(proj.path().join("target.txt"), "file-body").unwrap();
        let cmds = proj.path().join(".claude").join("commands");
        fs::create_dir_all(&cmds).unwrap();
        // allowed-tools is present and excludes fs.read, so the @target.txt injection
        // must fail closed (fs.read is not in the narrowed registry).
        fs::write(
            cmds.join("peek.md"),
            "---\nallowed-tools: fs.write\n---\nShow @target.txt\n",
        )
        .unwrap();

        let res = run_command_in(
            "peek",
            &[],
            proj.path().to_path_buf(),
            home.path().to_path_buf(),
        )
        .await;
        assert!(
            res.is_err(),
            "expected fail-closed @-injection under a narrowed allowlist, got: {res:?}"
        );
        assert!(
            res.unwrap_err()
                .to_string()
                .contains("file injection `@target.txt`"),
            "expected the fs.read injection to be the failure cause"
        );
    }

    #[tokio::test]
    async fn run_path_registry_applies_discovered_permissions() {
        use otto_workspace::LocalWorkspace;
        use serde_json::json;
        use std::sync::Arc;

        let proj = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let claude = proj.path().join(".claude");
        std::fs::create_dir_all(&claude).unwrap();
        std::fs::write(
            claude.join("settings.json"),
            r#"{ "permissions": { "deny": ["Write(dist/**)"] } }"#,
        )
        .unwrap();

        let ext = otto_extensions::discover(proj.path(), home.path());
        assert!(!ext.permissions.is_empty());

        let ws: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(proj.path().to_path_buf()));
        // Mirrors the gate-selection logic in `build_tools_preferring_mcp`; keep in sync.
        let reg = if !ext.permissions.is_empty() {
            // The `otto run` spine never sets approve_edits.
            otto_engine::build_tool_registry_with_permissions(
                ws,
                proj.path().to_path_buf(),
                &ext.permissions,
                false,
            )
        } else {
            otto_engine::build_tool_registry(ws, proj.path().to_path_buf())
        };

        let err = reg
            .call("fs.write", json!({"path": "dist/x.txt", "contents": "hi"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("denied by permission gate"));
    }

    #[tokio::test]
    async fn serve_path_registry_composes_permissions_with_approval_mode() {
        use otto_engine_core::tool::Decision;
        use otto_workspace::LocalWorkspace;
        use serde_json::json;
        use std::sync::Arc;

        let proj = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let claude = proj.path().join(".claude");
        std::fs::create_dir_all(&claude).unwrap();
        std::fs::write(
            claude.join("settings.json"),
            r#"{ "permissions": { "deny": ["Write(dist/**)"] } }"#,
        )
        .unwrap();

        let ext = otto_extensions::discover(proj.path(), home.path());
        assert!(!ext.permissions.is_empty());

        let ws: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(proj.path().to_path_buf()));
        let approve_edits = true;
        // Mirrors the gate-selection logic in `build_tools_preferring_mcp` (which `cmd_serve`
        // calls with the discovered permissions + the --approve-edits flag); keep in sync.
        let reg = if !ext.permissions.is_empty() {
            otto_engine::build_tool_registry_with_permissions(
                ws,
                proj.path().to_path_buf(),
                &ext.permissions,
                approve_edits,
            )
        } else if approve_edits {
            otto_engine::build_tool_registry_approving(ws, proj.path().to_path_buf())
        } else {
            otto_engine::build_tool_registry(ws, proj.path().to_path_buf())
        };

        // An ordinary write is upgraded to Ask for interactive approval, not silently applied.
        assert_eq!(
            reg.check("fs.write", &json!({"path": "src/x.rs"})),
            Decision::Ask
        );
        // A rule-driven deny still wins over approval mode.
        assert_eq!(
            reg.check("fs.write", &json!({"path": "dist/x.txt"})),
            Decision::Deny
        );
    }

    #[tokio::test]
    async fn build_serve_tools_wraps_hooks_around_permission_and_approval_gate() {
        use otto_workspace::LocalWorkspace;

        if !otto_tools::os_sandbox_available() {
            eprintln!("skipping serve hooks composition test: no OS sandbox backend");
            return;
        }
        let proj = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        std::fs::write(proj.path().join("target.txt"), "hi").unwrap();
        let claude = proj.path().join(".claude");
        std::fs::create_dir_all(&claude).unwrap();
        std::fs::write(
            claude.join("settings.json"),
            r#"{
                "permissions": { "deny": ["Write(dist/**)"] },
                "hooks": { "PreToolUse": [
                    {"matcher": "fs.read", "hooks": [{"type": "command", "command": "exit 2"}]}
                ] }
            }"#,
        )
        .unwrap();

        let ext = otto_extensions::discover(proj.path(), home.path());
        assert!(!ext.permissions.is_empty());
        assert!(!ext.hooks.is_empty());

        let ws: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(proj.path().to_path_buf()));
        let (tools, _conns) =
            super::build_serve_tools(&ext, ws, proj.path().to_path_buf(), true).await;

        // The hook fires even though fs.read is otherwise allowed by the permission/approval gate.
        let err = tools
            .call("fs.read", serde_json::json!({ "path": "target.txt" }))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("blocked by PreToolUse hook"),
            "got: {err}"
        );
        // The permission-gate deny still wins for an unrelated tool call (composition intact).
        assert_eq!(
            tools.check("fs.write", &serde_json::json!({"path": "dist/x.txt"})),
            otto_engine_core::tool::Decision::Deny
        );
    }
}
