//! `otto run "<goal>" [--root <path>] [--agent <name>]` — run a single turn (or a named custom agent) and print output.
//! `otto serve [--root <path>] [--port <p>] [--ui-dir <path>] [--approve-edits] [--single-user | --promotion-receiver] [--promote-loopback | --promote-vps <ws-endpoint> | --promote-microvm | --promote-fly] [--accept-promotions]` — serve over WebSocket. Three auth modes (spec §6.5): the default `Users` (TOTP via `otto auth`), `--single-user` (loopback-only, no credential — the desktop sidecar), and `--promotion-receiver` (machine-to-machine, requires `--accept-promotions`). The promotion secret is `OTTO_PROMOTION_SECRET` (renamed from `OTTO_TOKEN`), required only when a `--promote-*`/`--accept-promotions`/`--promotion-receiver` flag is set.
//! `otto auth enroll <user>` / `otto auth list` / `otto auth revoke <user>` — provision/remove TOTP principals against the `OTTO_AUTH_DB` store (the out-of-band bootstrap, spec §7.4).
//! `otto plugin marketplace add|remove|update|list` / `otto plugin install|uninstall|list` — manage Claude Code plugin marketplaces under `~/.claude/plugins/marketplaces/`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rand::RngCore;

use otto_engine::{
    build_composed_tools, build_router, resolve_tls_paths, run_goal, serve_app_with_base, serve_run,
};
// The composition helpers now live in the library (so the `cli` crate can reach them); this
// binary drives them only through `build_composed_tools`, but its test module exercises them
// directly, so the names must be in scope for `super::` there.
#[cfg(test)]
use otto_engine::{register_hooks, register_skills};
use otto_engine_core::traits::Workspace;
use otto_workspace::LocalWorkspace;

use otto_auth::AuthStore;

mod plugin_cli;
mod plugin_tui;

const VERSION: &str = env!("CARGO_PKG_VERSION");

const USAGE: &str = "usage:
  otto run \"<goal>\" [--root <path>] [--agent <name>]
  otto serve [--root <path>] [--port <p>] [--ui-dir <path>] [--approve-edits] [--single-user | --promotion-receiver] [--promote-loopback | --promote-vps <ws-endpoint> | --promote-microvm | --promote-fly] [--accept-promotions]
  otto auth enroll <user> [--force]
  otto auth list
  otto auth revoke <user>
  otto plugin                                  interactive TUI (default)
  otto plugin marketplace add|remove|update|list
  otto plugin install|uninstall|list
  otto --version | -V
  otto --help | -h";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let command = args.next().unwrap_or_default();
    let rest: Vec<String> = args.collect();

    match command.as_str() {
        "run" => cmd_run(rest).await,
        "serve" => cmd_serve(rest).await,
        "auth" => cmd_auth(rest).await,
        "plugin" => plugin_cli::cmd_plugin(rest, home_dir()).await,
        "version" | "--version" | "-V" => {
            println!("otto {VERSION}");
            Ok(())
        }
        "help" | "--help" | "-h" => {
            println!("{USAGE}");
            Ok(())
        }
        _ => {
            eprintln!(
                "{}\n",
                otto_engine::banner::banner(otto_engine::banner::ColorMode::detect())
            );
            eprintln!("{USAGE}");
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

/// Parse `--ui-dir <path>` from serve args. `None` means the static UI route is not installed.
///
/// SECURITY: deliberately no default and no env fallback. `ServeDir` does not consult the
/// sensitive-path floor, so a defaulted or inferred value pointing at a workspace root would
/// serve `.env`/`.ssh/` over plain HTTP. See `serve::with_ui_dir`. Deployments that configure
/// this through the environment pass it as a flag from their launcher — as
/// `deploy/fly/Dockerfile`'s `CMD` already does for `OTTO_PORT` and `OTTO_ROOT`.
fn parse_ui_dir(args: &[String]) -> Option<PathBuf> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--ui-dir" {
            match it.next() {
                Some(p) => return Some(PathBuf::from(p)),
                None => {
                    eprintln!("error: --ui-dir requires a path");
                    std::process::exit(2);
                }
            }
        }
    }
    None
}

/// Validate a `--ui-dir` value before it can ever reach `ServeDir`, returning the canonicalized
/// path on success.
///
/// SECURITY (closes a real vulnerability, not a hypothetical one): `ServeDir::new(dir)` resolves
/// every request path relative to `dir` with **no validation of its own** — an empty `dir`
/// resolves relative to the process's current working directory, and a nonexistent/non-directory
/// `dir` silently 404s everything while the CLI still reports success. Both were reproduced
/// end-to-end against a release binary: `--ui-dir ""` served `.env` and `.ssh/id_rsa` over
/// unauthenticated plain HTTP (see the design spec / final-fix report for the transcript).
///
/// This check lives in its own pure, testable function — not inlined into `parse_ui_dir` (which
/// has no I/O and stays a cheap arg-extraction helper) and not deferred into `with_ui_dir` (by the
/// time that runs, the caller has already decided to install the route and log success; validating
/// there would still let a bad value reach `ServeDir` in tests/other callers that build the app
/// directly). Doing it once, right after parsing, in `cmd_serve`, means: (1) a bad value is a hard,
/// fail-closed error before any other server setup runs, and (2) canonicalizing here means the
/// served base can never be reinterpreted later relative to a changed process CWD.
fn validate_ui_dir(dir: &Path) -> Result<PathBuf, String> {
    if dir.as_os_str().is_empty() {
        return Err("--ui-dir must not be empty".to_string());
    }
    let canonical = std::fs::canonicalize(dir)
        .map_err(|e| format!("--ui-dir {dir:?} does not exist or is not accessible: {e}"))?;
    if !canonical.is_dir() {
        return Err(format!("--ui-dir {dir:?} is not a directory"));
    }
    Ok(canonical)
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

/// The auth database path: `OTTO_AUTH_DB` if set, else the OS data dir + `otto/auth.db` (spec A5).
/// Auth state — TOTP secrets, the HS256 signing key, refresh-token hashes — is a credential and
/// lives outside the workspace so the sensitive-path floor is untouched (the session store's
/// `otto-sessions.db` default is inside the workspace by contrast). `otto auth` and `otto serve`
/// share this path.
///
/// **Fails closed (spec A5)** when neither the variable nor the OS data dir is available: there
/// is deliberately no CWD fallback. Writing the credentials into an arbitrary directory — e.g. a
/// workspace root a serve was started inside — would place them where the sensitive-path floor
/// does not cover them, so `otto auth`/`otto serve` refuse to run instead.
fn auth_db_path() -> anyhow::Result<String> {
    auth_db_path_from(std::env::var("OTTO_AUTH_DB").ok(), dirs::data_dir())
}

/// The pure core of [`auth_db_path`]: an explicit `OTTO_AUTH_DB` wins; otherwise the OS data
/// dir is required, or the caller fails closed with an actionable error. Tested directly so the
/// no-CWD-fallback invariant is provable without manipulating process-global env/dirs.
fn auth_db_path_from(env: Option<String>, data_dir: Option<PathBuf>) -> anyhow::Result<String> {
    if let Some(path) = env {
        return Ok(path);
    }
    let dir = data_dir.ok_or_else(|| {
        anyhow::anyhow!(
            "cannot determine the OS data directory for the auth database and OTTO_AUTH_DB is \
             unset: set OTTO_AUTH_DB to an explicit path outside any workspace, or the auth \
             state (TOTP secrets, signing key, refresh hashes) cannot be placed safely"
        )
    })?;
    Ok(dir
        .join("otto")
        .join("auth.db")
        .to_string_lossy()
        .into_owned())
}

/// Parse `host` as an `IpAddr` and require `is_loopback()`. The `--single-user` bind guard (spec
/// §6.5): "loopback" is a predicate, not a string match. A value that does not parse as an IP is
/// rejected — so `localhost` is refused (it resolves, but not necessarily to a loopback address),
/// `::1` and `127.0.0.2` are accepted, and `0.0.0.0` is refused. Pure, per §9.1.
fn resolve_bind_host_is_loopback(host: &str) -> bool {
    host.parse::<std::net::IpAddr>()
        .map(|ip| ip.is_loopback())
        .unwrap_or(false)
}

/// Whether a `--single-user` serve's promote posture is legal (spec §6.5): the only promote mode
/// permitted is `--promote-loopback` — it provisions a second in-process engine in the same trust
/// domain and needs no cross-machine credential — and a plain serve (no promote flags at all) is
/// equally valid. `--promote-vps`/`--promote-microvm`/`--promote-fly` and `--accept-promotions`/
/// `--promotion-receiver` are startup errors. `loopback` is the one permitted mode, so it needs no
/// explicit test; the predicate is false exactly when a disallowed mode is present.
fn single_user_promote_modes_ok(
    _loopback: bool,
    vps: bool,
    microvm: bool,
    fly: bool,
    accept_promotions: bool,
) -> bool {
    !vps && !microvm && !fly && !accept_promotions
}

/// Refuse to start a `Users`-mode serve with zero enrolled principals, naming `otto auth enroll`
/// (spec §7.4): a server nobody can log into is always a misconfiguration, and failing at startup
/// beats failing at first connection. Scoped to `AuthMode::Users` by the caller — `SingleUser` and
/// `Machine` have no enrolled principals by design (§6.5).
async fn users_mode_has_enrolled_principals(store: &dyn AuthStore) -> anyhow::Result<()> {
    if store.enrolled_count().await? == 0 {
        anyhow::bail!(
            "no enrolled principals: run `otto auth enroll <user>` to provision the first \
             principal before starting a multi-user server"
        );
    }
    Ok(())
}

/// The `--promotion-receiver` startup preconditions (spec §6.5): `--accept-promotions` must be set,
/// and no principal may be enrolled — a machine-credential host cannot double as a multi-user
/// server, or the machine secret would be a backdoor into it. `enrolled` is passed in so the guard
/// stays pure (the caller opens the store).
fn promotion_receiver_preconditions(accept_promotions: bool, enrolled: u64) -> anyhow::Result<()> {
    if !accept_promotions {
        anyhow::bail!("--promotion-receiver requires --accept-promotions");
    }
    if enrolled > 0 {
        anyhow::bail!(
            "--promotion-receiver refuses to start with {enrolled} enrolled principal(s): a \
             machine-credential host cannot double as a multi-user server"
        );
    }
    Ok(())
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

/// Read Fly provisioning parameters from `OTTO_FLY_*` / `FLY_API_TOKEN`. Missing `FLY_API_TOKEN`
/// yields an empty token; provisioning then fails at the first API call with a clear 401 — the CLI
/// need not special-case it here.
fn fly_config_from_env() -> otto_engine::FlyConfig {
    fn num(key: &str, default: u32) -> u32 {
        std::env::var(key)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    }
    otto_engine::FlyConfig {
        api_token: std::env::var("FLY_API_TOKEN").unwrap_or_default(),
        org_slug: std::env::var("OTTO_FLY_ORG").unwrap_or_else(|_| "personal".to_string()),
        region: std::env::var("OTTO_FLY_REGION").unwrap_or_else(|_| "iad".to_string()),
        image: std::env::var("OTTO_FLY_IMAGE").unwrap_or_default(),
        vm_cpus: num("OTTO_FLY_CPUS", 1),
        vm_cpu_kind: std::env::var("OTTO_FLY_CPU_KIND").unwrap_or_else(|_| "shared".to_string()),
        vm_mem_mib: num("OTTO_FLY_MEM_MIB", 1024),
        app_prefix: std::env::var("OTTO_FLY_APP_PREFIX")
            .unwrap_or_else(|_| "otto-session".to_string()),
        internal_port: num("OTTO_FLY_PORT", 8787) as u16,
        boot_timeout: std::time::Duration::from_millis(
            num("OTTO_FLY_BOOT_TIMEOUT_MS", 30_000) as u64
        ),
        api_base: std::env::var("OTTO_FLY_API_BASE")
            .unwrap_or_else(|_| "https://api.machines.dev/v1".to_string()),
        graphql_base: std::env::var("OTTO_FLY_GRAPHQL_BASE")
            .unwrap_or_else(|_| "https://api.fly.io/graphql".to_string()),
        public_base_override: std::env::var("OTTO_FLY_PUBLIC_BASE").ok(),
    }
}

async fn cmd_run(args: Vec<String>) -> anyhow::Result<()> {
    // A bad *_BASE_URL makes the engine degrade to the offline canned provider, which still
    // produces a complete-looking turn. In the CLI — where a human set the variable — refuse to
    // start instead, so the misconfiguration is obvious in seconds rather than days.
    otto_engine::preflight_base_urls()?;
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
    // the gate can be a PolicyGate. build_composed_tools then layers skills, hooks, and bundled
    // plugin MCP servers on top — the same composition `otto serve` and the --command/--agent
    // subpaths use.
    let ext = otto_extensions::discover(&root, &home_dir());
    let (tools, _mcp_conns) =
        build_composed_tools(&ext, tools_workspace, root.clone(), false).await;
    // _mcp_conns is held until end of function so the mcp children stay alive.
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

    let mut registry = AgentRegistry::new();
    let mut allowlists: HashMap<String, Option<Vec<String>>> = HashMap::new();
    // Pin the remote model to the top-level `--agent`'s `model:` field. Nested Task
    // sub-dispatches inherit this same pinned router (per-sub-agent model is deferred).
    let mut model_override: Option<String> = None;
    for def in &ext.agents {
        if def.name == name {
            model_override = def.model.clone();
        }
        allowlists.insert(def.name.clone(), def.tools.clone());
        registry.register(
            Role::Custom(def.name.clone()),
            Arc::new(MarkdownAgent::new(def.clone())),
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
    // The same composed registry the spine/serve paths use (permissions PolicyGate, skills,
    // hooks, plugin MCP servers). TaskTool then narrows it per-agent via `tools:` allowlists —
    // mirroring serve's run_agent_with_controls, which hands TaskTool the server's composed
    // registry. _mcp is held until end of function so the mcp children stay alive.
    let (base_tools, _mcp) = build_composed_tools(&ext, tools_ws, root, false).await;

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
    let def = ext
        .commands
        .iter()
        .find(|c| c.name == name)
        .cloned()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no command named '{name}' in ~/.claude/commands/ or {}/.claude/commands/",
                root.display()
            )
        })?;

    // The composed tool registry: the same permissions/skills/hooks/plugin-MCP composition the
    // spine and serve paths use (Slices 6–11 + this slice). Injection reaches fs.read/bash
    // through the same gate the spine turn uses. Reused as the turn's tools.
    let tools_workspace: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(root.clone()));
    let (tools, _mcp_conns) =
        build_composed_tools(&ext, tools_workspace, root.clone(), false).await;
    // _mcp_conns is held until end of function so the mcp children stay alive.
    // Narrow the composed registry to the command's allowed-tools (None = all tools) before it
    // is used for BOTH injection resolution and the spine turn — so a disallowed tool is
    // fail-closed.
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

/// Parse a `UserId` from a CLI argument, exiting with an error on an invalid id (never echoing the
/// rejected value — it is operator input, but `InvalidUserId`'s message already omits it).
fn parse_user(s: &str) -> otto_protocol::UserId {
    match otto_protocol::UserId::parse(s) {
        Ok(user) => user,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(2);
        }
    }
}

/// Parse a `UserId`, refusing the reserved `local` principal (spec §7.4). All three `otto auth`
/// subcommands refuse it — `enroll local` cannot create it, `revoke local` cannot delete it, and
/// `list` never shows it as though it were an enrolled principal — so the reserved built-in can
/// never become half-real.
fn refuse_local(s: &str) -> otto_protocol::UserId {
    if s == otto_protocol::UserId::local().as_str() {
        eprintln!(
            "error: 'local' is the reserved built-in principal and cannot be enrolled or revoked"
        );
        std::process::exit(2);
    }
    parse_user(s)
}

/// Enroll `user`: generate a 20-byte TOTP secret (spec §6.1), persist it, and print the
/// `otpauth://` URI plus a terminal QR (spec §7.4). Enrolling an already-enrolled user re-provisions
/// them — invalidating the old secret — and requires `--force` so a typo cannot silently lock
/// someone out.
async fn enroll_user(
    store: &otto_auth::SqliteAuthStore,
    user: &otto_protocol::UserId,
    force: bool,
) -> anyhow::Result<()> {
    if store.totp_secret(user).await?.is_some() {
        if !force {
            anyhow::bail!(
                "{user} is already enrolled; pass --force to re-provision (this invalidates the \
                 existing secret)"
            );
        }
        // Re-provision: drop the user and their refresh tokens first — `enroll_user` refuses a
        // duplicate, and this also resets the replay floor and the failure counter.
        store.revoke_user(user).await?;
    }
    let mut secret = [0u8; 20];
    rand::rngs::OsRng.fill_bytes(&mut secret);
    store.enroll_user(user, &secret).await?;
    let b32 = data_encoding::BASE32_NOPAD.encode(&secret);
    let uri = format!(
        "otpauth://totp/otto:{user}?secret={b32}&issuer=otto&algorithm=SHA1&digits=6&period=30"
    );
    println!("Enrolled {user}. Add the account to your authenticator app (or scan the QR):");
    println!("{uri}");
    let code = qrcode::QrCode::new(&uri)?;
    let qr = code
        .render::<qrcode::render::unicode::Dense1x2>()
        .module_dimensions(1, 1)
        .build();
    println!("{qr}");
    Ok(())
}

/// `otto auth enroll <user> [--force]` / `otto auth list` / `otto auth revoke <user>` — the
/// out-of-band bootstrap that provisions the first principal (spec §7.4). Runs on the host against
/// the `OTTO_AUTH_DB` store directly; it needs no server and no credential, which is precisely why
/// it is the bootstrap.
async fn cmd_auth(args: Vec<String>) -> anyhow::Result<()> {
    let mut it = args.into_iter();
    let sub = it.next().unwrap_or_default();
    match sub.as_str() {
        "enroll" => {
            let mut user: Option<String> = None;
            let mut force = false;
            for a in it {
                if a == "--force" {
                    force = true;
                } else if user.is_none() {
                    user = Some(a);
                } else {
                    eprintln!(
                        "error: otto auth enroll takes exactly one user (plus optional --force)"
                    );
                    std::process::exit(2);
                }
            }
            let Some(user) = user else {
                eprintln!("error: otto auth enroll requires a user");
                std::process::exit(2);
            };
            let user = refuse_local(&user);
            let store = otto_auth::SqliteAuthStore::open(auth_db_path()?).await?;
            enroll_user(&store, &user, force).await
        }
        "list" => {
            let store = otto_auth::SqliteAuthStore::open(auth_db_path()?).await?;
            let users = store.list_users().await?;
            if users.is_empty() {
                println!("no enrolled principals");
            } else {
                // `list_users` is ordered by id; `local` is never stored, so no filter needed.
                for user in users {
                    println!("{user}");
                }
            }
            Ok(())
        }
        "revoke" => {
            let user = match it.next() {
                Some(user) => refuse_local(&user),
                None => {
                    eprintln!("error: otto auth revoke requires a user");
                    std::process::exit(2);
                }
            };
            let store = otto_auth::SqliteAuthStore::open(auth_db_path()?).await?;
            store.revoke_user(&user).await?;
            println!("revoked {user}");
            Ok(())
        }
        _ => {
            eprintln!(
                "usage: otto auth enroll <user> [--force] | otto auth list | otto auth revoke <user>"
            );
            std::process::exit(2);
        }
    }
}

async fn cmd_serve(args: Vec<String>) -> anyhow::Result<()> {
    // Fail fast on a bad *_BASE_URL. This matters most on serve: the offline fallback keeps
    // answering, so connected clients would receive canned output indefinitely with nothing but a
    // single startup warning on the server's stderr to show for it.
    otto_engine::preflight_base_urls()?;
    let (root, positional) = parse_root(&args);
    // Validated (and canonicalized) immediately after parsing, before any other server setup, so
    // a bad --ui-dir fails fast and closed rather than starting a server that looks healthy and
    // either 404s everything or — worse — serves the process CWD. See `validate_ui_dir`.
    let ui_dir = match parse_ui_dir(&positional) {
        Some(raw) => match validate_ui_dir(&raw) {
            Ok(dir) => Some(dir),
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(2);
            }
        },
        None => None,
    };
    let mut port: u16 = std::env::var("OTTO_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(7878);
    let mut tls_cert: Option<PathBuf> = None;
    let mut tls_key: Option<PathBuf> = None;
    let mut approve_edits = false;
    let mut promote_loopback = false;
    let mut accept_promotions = false;
    let mut single_user = false;
    let mut promotion_receiver = false;
    let mut promote_vps: Option<String> = None;
    let mut promote_microvm = false;
    let mut promote_fly = false;
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
            "--single-user" => single_user = true,
            "--promotion-receiver" => promotion_receiver = true,
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
            "--promote-fly" => promote_fly = true,
            _ => {}
        }
    }

    // ---- Auth posture (spec §6.5): three explicit modes, resolved before any server setup so a
    // misconfiguration is a hard, fail-closed startup error rather than a server that starts and
    // accepts or refuses the wrong connections.
    //
    //   --single-user        → SingleUser: loopback bind enforced, no authenticator, no auth
    //                          database; only --promote-loopback is allowed.
    //   --promotion-receiver → Machine: requires --accept-promotions and refuses to start with any
    //                          principal enrolled (the machine secret must never be a backdoor
    //                          into a multi-user server).
    //   default              → Users: a real TotpAuthenticator over the OTTO_AUTH_DB store, and the
    //                          zero-principal refusal (spec §7.4).
    //
    // The promotion secret (OTTO_PROMOTION_SECRET, renamed from OTTO_TOKEN) is required only when a
    // --promote-* / --accept-promotions / --promotion-receiver flag is set — a serve with neither
    // needs no shared secret. --single-user never reads it: its only allowed promote (loopback)
    // provisions an in-process engine in the same trust domain, which needs no cross-machine
    // credential, and the desktop sidecar runs without any secret env.
    let needs_secret = (promote_loopback
        || promote_vps.is_some()
        || promote_microvm
        || promote_fly
        || accept_promotions
        || promotion_receiver)
        && !single_user;
    let promotion_secret = if needs_secret {
        match std::env::var("OTTO_PROMOTION_SECRET") {
            Ok(t) if !t.is_empty() => t,
            _ => {
                eprintln!(
                    "error: OTTO_PROMOTION_SECRET must be set when using --promote-*, \
                     --accept-promotions, or --promotion-receiver"
                );
                std::process::exit(2);
            }
        }
    } else {
        String::new()
    };

    let auth = if single_user {
        let bind_host = std::env::var("OTTO_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
        if !resolve_bind_host_is_loopback(&bind_host) {
            eprintln!(
                "error: --single-user requires a loopback bind host, but OTTO_HOST={bind_host:?} \
                 is not a loopback address (localhost is refused because it resolves, but not \
                 necessarily to a loopback address)"
            );
            std::process::exit(2);
        }
        // `--promotion-receiver` implies `--accept-promotions`; fold it in so the flag is refused
        // under --single-user even when its own `--accept-promotions` requirement is unmet.
        if !single_user_promote_modes_ok(
            promote_loopback,
            promote_vps.is_some(),
            promote_microvm,
            promote_fly,
            accept_promotions || promotion_receiver,
        ) {
            eprintln!(
                "error: --single-user allows only --promote-loopback; --promote-vps, \
                 --promote-microvm, --promote-fly, --accept-promotions, and --promotion-receiver \
                 are refused"
            );
            std::process::exit(2);
        }
        otto_engine_core::auth::AuthConfig {
            mode: otto_protocol::AuthMode::SingleUser,
            authenticator: None,
            promotion_secret: None,
            handshake_deadline: std::time::Duration::from_secs(10),
        }
    } else if promotion_receiver {
        let store = otto_auth::SqliteAuthStore::open(auth_db_path()?).await?;
        let enrolled = store.enrolled_count().await?;
        if let Err(e) = promotion_receiver_preconditions(accept_promotions, enrolled) {
            eprintln!("error: {e}");
            std::process::exit(2);
        }
        otto_engine_core::auth::AuthConfig {
            mode: otto_protocol::AuthMode::Machine,
            authenticator: None,
            promotion_secret: Some(promotion_secret.clone()),
            handshake_deadline: std::time::Duration::from_secs(10),
        }
    } else {
        // Users (the default): TOTP-authenticated, ≥1 enrolled principal required (§7.4).
        let store = Arc::new(otto_auth::SqliteAuthStore::open(auth_db_path()?).await?);
        if let Err(e) = users_mode_has_enrolled_principals(store.as_ref()).await {
            eprintln!("error: {e}");
            std::process::exit(2);
        }
        let authenticator = Arc::new(otto_auth::TotpAuthenticator::new(
            store.clone(),
            Arc::new(otto_auth::SystemClock),
        ));
        otto_engine_core::auth::AuthConfig {
            mode: otto_protocol::AuthMode::Users,
            authenticator: Some(authenticator),
            promotion_secret: needs_secret.then(|| promotion_secret.clone()),
            handshake_deadline: std::time::Duration::from_secs(10),
        }
    };

    let ext = otto_extensions::discover(&root, &home_dir());

    let router: Arc<dyn otto_engine_core::Router> = Arc::from(build_router());
    let orch_workspace: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(root.clone()));
    let tools_workspace: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(root.clone()));
    // NOTE: permissions, hooks, skills, and plugin MCP servers are all enforced here via
    // `build_composed_tools` (composed with --approve-edits when both are configured; see
    // build_tool_registry_inner / register_skills / register_hooks). `--command` and `--agent`
    // are wired on serve via `EngineService::with_extensions` (`run_command_with_controls` /
    // `run_agent_with_controls`).
    let (tools, _mcp_conns) =
        build_composed_tools(&ext, tools_workspace, root.clone(), approve_edits).await;
    let tools = Arc::new(tools);
    let store: Arc<dyn otto_persistence::SessionStore> =
        Arc::new(otto_persistence::SqliteStore::open(&open_db_path()).await?);
    let registry = Arc::new(otto_engine::build_default_registry());

    let retriever = otto_engine::build_retriever(&root).await;
    let service = otto_engine::EngineService::new(store, registry, router, orch_workspace, tools)
        .with_retriever(retriever)
        .with_extensions(Arc::new(ext));
    let capabilities = otto_engine::build_capabilities();
    let promote = match (
        promote_loopback,
        promote_vps.clone(),
        promote_microvm,
        promote_fly,
    ) {
        (l, v, m, f) if (l as u8) + (v.is_some() as u8) + (m as u8) + (f as u8) > 1 => {
            eprintln!(
                "error: --promote-loopback, --promote-vps, --promote-microvm, and --promote-fly are mutually exclusive"
            );
            std::process::exit(2);
        }
        (true, _, _, _) => Some(otto_engine::PromoteConfig {
            token: promotion_secret.clone(),
            // The dot-prefix is load-bearing: `LocalWorkspace::list` skips dot-directories, so a
            // provisioned engine's restored store/workspace under here is never recursively
            // captured by a later `workspace.snapshot()`. Do not rename without that guarantee.
            mode: otto_engine::PromoteMode::Loopback {
                base_dir: root.join(".otto-remotes"),
            },
        }),
        (_, Some(endpoint), _, _) => Some(otto_engine::PromoteConfig {
            token: promotion_secret.clone(),
            mode: otto_engine::PromoteMode::Vps { endpoint },
        }),
        (_, _, true, _) => Some(otto_engine::PromoteConfig {
            token: promotion_secret.clone(),
            mode: otto_engine::PromoteMode::Microvm {
                config: microvm_config_from_env(),
            },
        }),
        (_, _, _, true) => Some(otto_engine::PromoteConfig {
            token: promotion_secret.clone(),
            mode: otto_engine::PromoteMode::Fly {
                config: fly_config_from_env(),
            },
        }),
        (false, None, false, false) => None,
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
        auth,
        capabilities,
        promote,
        accept_promotions,
        public_ws_base,
    );
    // Static UI route: installed only when the operator passed --ui-dir. Absent by default —
    // see `serve::with_ui_dir` for why this must never be defaulted or inferred.
    let app = match ui_dir {
        Some(dir) => {
            eprintln!("otto serve serving web UI from {}", dir.display());
            otto_engine::serve_with_ui_dir(app, dir)
        }
        None => app,
    };

    // Bind host: default loopback (safe for local/dev — never expose a local serve to the network).
    // Set OTTO_HOST=0.0.0.0 to accept off-host connections, as the Fly deploy image does so Fly's
    // proxy (which reaches the machine over its network interface, not loopback) can reach otto.
    let bind_host = std::env::var("OTTO_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
    let addr = format!("{bind_host}:{port}");
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

    #[test]
    fn parse_ui_dir_extracts_path() {
        let args = vec![
            "--port".to_string(),
            "9000".to_string(),
            "--ui-dir".to_string(),
            "/srv/otto-ui".to_string(),
        ];
        assert_eq!(parse_ui_dir(&args), Some(PathBuf::from("/srv/otto-ui")));
    }

    /// The security-relevant default: absent flag means the static route is never installed.
    /// There is deliberately no default path and no env fallback — see `serve::with_ui_dir`.
    #[test]
    fn parse_ui_dir_absent_is_none() {
        let args = vec!["--port".to_string(), "9000".to_string()];
        assert_eq!(parse_ui_dir(&args), None);
    }

    /// C1: an empty `--ui-dir` value must be a hard error, never accepted as "the process CWD".
    #[test]
    fn validate_ui_dir_rejects_empty() {
        let err = validate_ui_dir(Path::new("")).unwrap_err();
        assert!(err.contains("must not be empty"), "unexpected error: {err}");
    }

    /// C1 / I2: a nonexistent path must be a hard error, not a server that starts and 404s.
    #[test]
    fn validate_ui_dir_rejects_nonexistent_path() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");
        let err = validate_ui_dir(&missing).unwrap_err();
        assert!(err.contains("does not exist"), "unexpected error: {err}");
    }

    /// C1: a file (not a directory) must be rejected — `ServeDir` needs a directory.
    #[test]
    fn validate_ui_dir_rejects_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("not-a-dir");
        std::fs::write(&file, b"hello").unwrap();
        let err = validate_ui_dir(&file).unwrap_err();
        assert!(
            err.contains("is not a directory"),
            "unexpected error: {err}"
        );
    }

    /// C1: a valid, existing directory is still accepted, canonicalized.
    #[test]
    fn validate_ui_dir_accepts_a_valid_directory() {
        let dir = tempfile::tempdir().unwrap();
        let canonical = validate_ui_dir(dir.path()).unwrap();
        assert_eq!(canonical, dir.path().canonicalize().unwrap());
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

    #[tokio::test]
    async fn run_custom_agent_dispatches_with_full_extensions_configured() {
        use std::fs;
        // Permissions + skills + hooks all configured: the agent path must build its composed
        // registry (PolicyGate, skill tool, hook wrap) and still complete an offline one-shot
        // dispatch. Guards the --agent composition against breaking the deterministic path.
        let proj = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let claude = proj.path().join(".claude");
        let agents = claude.join("agents");
        let skill_dir = claude.join("skills").join("greeter");
        fs::create_dir_all(&agents).unwrap();
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            claude.join("settings.json"),
            r#"{
                "permissions": { "deny": ["Write(dist/**)"] },
                "hooks": { "PostToolUse": [
                    {"matcher": "fs.read", "hooks": [{"type": "command", "command": "true"}]}
                ] }
            }"#,
        )
        .unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: greeter\ndescription: greets\n---\nSay hi.\n",
        )
        .unwrap();
        fs::write(
            agents.join("echoer.md"),
            "---\nname: echoer\ndescription: echoes\ntools: fs.read\n---\nYou are an echo agent.\n",
        )
        .unwrap();

        let ok = run_custom_agent_in(
            "echoer",
            "hello",
            proj.path().to_path_buf(),
            home.path().to_path_buf(),
        )
        .await;
        assert!(
            ok.is_ok(),
            "expected dispatch to succeed under full composition: {ok:?}"
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
    async fn command_composition_exposes_skill_tool_until_narrowed_away() {
        use std::fs;
        // A discovered skill registers the gated `skill` tool in the composed registry a
        // command uses; a present `allowed-tools` that omits `skill` narrows it away.
        let proj = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let skill_dir = proj.path().join(".claude").join("skills").join("greeter");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: greeter\ndescription: greets\n---\nSay hi.\n",
        )
        .unwrap();

        let ext = otto_extensions::discover(proj.path(), home.path());
        let ws: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(proj.path().to_path_buf()));
        let (tools, _conns) =
            super::build_composed_tools(&ext, ws, proj.path().to_path_buf(), false).await;

        // No allowed-tools (None) → the composed registry keeps `skill`.
        let kept = narrow_for_command(tools, &None);
        assert!(kept.tool_names().iter().any(|n| n == "skill"));

        // allowed-tools present but omitting `skill` → narrowed away. (Note: there is no
        // `Skill`→`skill` alias in permission_def::normalize_tool — lowercase only.)
        let narrowed = narrow_for_command(kept, &Some(vec!["fs.read".to_string()]));
        assert!(!narrowed.tool_names().iter().any(|n| n == "skill"));
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
    async fn run_command_path_enforces_discovered_permissions() {
        use std::fs;
        // A settings.json `deny: Read(secret.txt)` rule must block the command's @-injection:
        // before CLI-composition parity, run_command_in built its registry with
        // PermissionRules::default() and this injection succeeded.
        let proj = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap(); // empty → no user-global config
        fs::write(proj.path().join("secret.txt"), "s3cr3t").unwrap();
        let claude = proj.path().join(".claude");
        let cmds = claude.join("commands");
        fs::create_dir_all(&cmds).unwrap();
        fs::write(
            claude.join("settings.json"),
            r#"{ "permissions": { "deny": ["Read(secret.txt)"] } }"#,
        )
        .unwrap();
        fs::write(cmds.join("peek.md"), "Show @secret.txt\n").unwrap();

        let res = run_command_in(
            "peek",
            &[],
            proj.path().to_path_buf(),
            home.path().to_path_buf(),
        )
        .await;
        assert!(
            res.is_err(),
            "expected the permissions deny rule to fail the @-injection closed, got: {res:?}"
        );
        let msg = res.unwrap_err().to_string();
        assert!(
            msg.contains("file injection `@secret.txt`"),
            "expected the fs.read injection to be the failure cause, got: {msg}"
        );
    }

    #[tokio::test]
    async fn run_command_path_fires_pretooluse_hook_on_injection() {
        use std::fs;
        if !otto_tools::os_sandbox_available() {
            eprintln!("skipping command-path hook test: no OS sandbox backend");
            return;
        }
        // A blocking PreToolUse hook on `bash` must abort the command's !`…` injection —
        // hooks now wrap the --command path's composed registry, same as spine/serve.
        let proj = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let claude = proj.path().join(".claude");
        let cmds = claude.join("commands");
        fs::create_dir_all(&cmds).unwrap();
        fs::write(
            claude.join("settings.json"),
            r#"{"hooks":{"PreToolUse":[{"matcher":"bash","hooks":[{"type":"command","command":"exit 2"}]}]}}"#,
        )
        .unwrap();
        fs::write(cmds.join("shell.md"), "Result: !`echo hi`\n").unwrap();

        let res = run_command_in(
            "shell",
            &[],
            proj.path().to_path_buf(),
            home.path().to_path_buf(),
        )
        .await;
        assert!(
            res.is_err(),
            "expected the PreToolUse hook to block the bash injection, got: {res:?}"
        );
        let msg = res.unwrap_err().to_string();
        assert!(
            msg.contains("command injection `!echo hi` failed")
                && msg.contains("blocked by PreToolUse hook"),
            "expected a hook-blocked bash injection, got: {msg}"
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

    // ---- Auth-mode guards (spec §6.5 / §7.4). Pure functions in the `validate_ui_dir` shape so
    // the security-relevant startup refusals ship with unit coverage. ----

    /// Finding 5: the auth DB must never fall back to the CWD. An explicit `OTTO_AUTH_DB` wins;
    /// the OS data dir is the default; neither available → fail closed with an actionable error
    /// (spec A5), so `otto serve`/`otto auth` refuse to run rather than write TOTP secrets and
    /// the signing key somewhere the sensitive-path floor does not cover.
    #[test]
    fn auth_db_path_fails_closed_without_a_data_dir() {
        // OTTO_AUTH_DB wins, even with a data dir present.
        assert_eq!(
            auth_db_path_from(Some("x.db".to_string()), Some(PathBuf::from("/data"))).unwrap(),
            "x.db"
        );
        // Absent env → the OS data dir + otto/auth.db (spec A5).
        assert_eq!(
            auth_db_path_from(None, Some(PathBuf::from("/data"))).unwrap(),
            "/data/otto/auth.db"
        );
        // Absent env AND no data dir → a hard, actionable error, never "otto-auth.db" in the CWD.
        let err = auth_db_path_from(None, None).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("OTTO_AUTH_DB"),
            "the error must name the escape hatch: {err}"
        );
        assert!(
            !msg.contains("otto-auth.db"),
            "the error must not suggest the CWD fallback: {err}"
        );
    }

    #[test]
    fn loopback_predicate_accepts_loopback_addresses() {
        assert!(resolve_bind_host_is_loopback("127.0.0.1"));
        assert!(resolve_bind_host_is_loopback("127.0.0.2"));
        assert!(resolve_bind_host_is_loopback("::1"));
    }

    #[test]
    fn loopback_predicate_rejects_non_loopback_and_non_ips() {
        assert!(!resolve_bind_host_is_loopback("0.0.0.0"));
        // `localhost` resolves, but not necessarily to a loopback address — refused per §6.5.
        assert!(!resolve_bind_host_is_loopback("localhost"));
        assert!(!resolve_bind_host_is_loopback("garbage"));
        assert!(!resolve_bind_host_is_loopback(""));
    }

    #[test]
    fn single_user_allows_only_loopback_promote() {
        // A plain --single-user serve, and the desktop sidecar's exact shape
        // (--single-user --promote-loopback), are both fine.
        assert!(single_user_promote_modes_ok(
            false, false, false, false, false
        ));
        assert!(single_user_promote_modes_ok(
            true, false, false, false, false
        ));
        // Every other promote mode is a startup error.
        assert!(!single_user_promote_modes_ok(
            false, true, false, false, false
        ));
        assert!(!single_user_promote_modes_ok(
            false, false, true, false, false
        ));
        assert!(!single_user_promote_modes_ok(
            false, false, false, true, false
        ));
        // --accept-promotions / --promotion-receiver are refused too.
        assert!(!single_user_promote_modes_ok(
            true, false, false, false, true
        ));
    }

    #[tokio::test]
    async fn users_mode_refuses_zero_enrolled_principals() {
        let dir = tempfile::tempdir().unwrap();
        let store = otto_auth::SqliteAuthStore::open(dir.path().join("auth.db"))
            .await
            .unwrap();
        let err = users_mode_has_enrolled_principals(&store)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("otto auth enroll"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn users_mode_passes_with_an_enrolled_principal() {
        use otto_auth::AuthStore;
        let dir = tempfile::tempdir().unwrap();
        let store = otto_auth::SqliteAuthStore::open(dir.path().join("auth.db"))
            .await
            .unwrap();
        store
            .enroll_user(&otto_protocol::UserId::parse("alice").unwrap(), &[1u8; 20])
            .await
            .unwrap();
        assert!(users_mode_has_enrolled_principals(&store).await.is_ok());
    }

    #[test]
    fn promotion_receiver_requires_accept_promotions() {
        let err = promotion_receiver_preconditions(false, 0).unwrap_err();
        assert!(
            err.to_string().contains("--accept-promotions"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn promotion_receiver_refuses_enrolled_principals() {
        let err = promotion_receiver_preconditions(true, 1).unwrap_err();
        assert!(
            err.to_string().contains("enrolled"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn promotion_receiver_is_ok_with_accept_and_zero_principals() {
        assert!(promotion_receiver_preconditions(true, 0).is_ok());
    }

    #[tokio::test]
    async fn enroll_force_reprovisions_and_invalidates_the_old_secret() {
        let dir = tempfile::tempdir().unwrap();
        let store = otto_auth::SqliteAuthStore::open(dir.path().join("auth.db"))
            .await
            .unwrap();
        let user = otto_protocol::UserId::parse("alice").unwrap();

        // First enrollment provisions a secret.
        store.enroll_user(&user, &[0u8; 20]).await.unwrap();
        let first = store.totp_secret(&user).await.unwrap().expect("enrolled");

        // Enrolling again without --force is refused (a typo cannot silently re-provision).
        let err = enroll_user(&store, &user, false).await.unwrap_err();
        assert!(
            err.to_string().contains("--force"),
            "unexpected error: {err}"
        );

        // With --force the secret is re-provisioned to a different value, so a stale
        // authenticator code from the old secret can no longer match (spec §7.4).
        enroll_user(&store, &user, true).await.unwrap();
        let second = store
            .totp_secret(&user)
            .await
            .unwrap()
            .expect("re-enrolled");
        assert_ne!(first, second, "re-provision must rotate the secret");
    }
}
