//! Engine wiring: assemble the default agent registry and run a turn end-to-end.

use std::path::PathBuf;
use std::sync::Arc;

use otto_agents::{Coder, ContextFinder, Planner, Verifier};
use otto_engine_core::tool::{
    AllowListAskResolver, AskResolver, DenyAsk, PermissionGate, ToolRegistry,
};
use otto_engine_core::traits::{Provider, Workspace, WorkspaceRead};
use otto_engine_core::{AgentRegistry, Router, TurnOutcome};
use otto_extensions::PermissionRules;
use otto_persistence::SessionStore;
use otto_protocol::{CapabilitiesManifest, Event, Role};
use otto_providers::{
    AnthropicProvider, DeepSeekProvider, GeminiProvider, LocalProvider, OllamaProvider,
    OpenAiProvider,
};
use otto_router::{BrainBlendRouter, PinnedModelRouter, SingleProviderRouter};
use otto_tools::{
    BashTool, DefaultPermissionGate, FsListTool, FsReadTool, FsWriteTool, SandboxPolicy,
    os_sandbox_available,
};

mod approval;
/// otto's terminal mark. Public so any tty frontend — and the `bake-art` example — can reach it.
pub mod banner;
mod hooks;
mod loopback;
mod mcp;
mod policy_gate;
mod serve;
mod service;

pub use approval::ApprovalModeGate;
pub use hooks::SandboxedHookExecutor;
pub use loopback::LoopbackTarget;
pub use mcp::{
    McpConnection, connect_bash as mcp_connect_bash, connect_fs as mcp_connect_fs,
    connect_git as mcp_connect_git, connect_grep as mcp_connect_grep,
    connect_lsp as mcp_connect_lsp, connect_plugin_server as mcp_connect_plugin_server,
};
pub use otto_remote::{
    FlyConfig, FlyTarget, MicrovmConfig, MicrovmTarget, PromoteBundle, PromoteConfig, PromoteMode,
    ProvisionedMachine, Provisioner, RemoteHandle, RemoteTarget, UnsupportedProvisioner, VpsTarget,
    export_bundle, mint_session_secret, promote,
};
pub use policy_gate::PolicyGate;
pub use serve::{
    app as serve_app, app_with_base as serve_app_with_base, resolve_tls_paths, run as serve_run,
    with_ui_dir as serve_with_ui_dir,
};
pub use service::{AcceptError, CollectingSink, EngineService, EventSink, TurnControls};

/// Default model ids when the corresponding `OTTO_*_MODEL` env var is unset. Referenced by
/// both `build_router` (selection) and `session_config` (recording the effective model).
const DEFAULT_OLLAMA_MODEL: &str = "llama3.2";
const DEFAULT_ANTHROPIC_MODEL: &str = "claude-haiku-4-5";
const DEFAULT_OPENAI_MODEL: &str = "gpt-4o-mini";
const DEFAULT_GEMINI_MODEL: &str = "gemini-2.5-flash";
const DEFAULT_DEEPSEEK_MODEL: &str = "deepseek-v4-flash";

/// Build the registry of built-in agents: the whole spine is real: LLM-backed Planner +
/// ContextFinder + Coder and a cargo-check Verifier. No stubs remain.
pub fn build_default_registry() -> AgentRegistry {
    let mut registry = AgentRegistry::new();
    registry.register(Role::Planner, Arc::new(Planner));
    registry.register(Role::ContextFinder, Arc::new(ContextFinder));
    registry.register(Role::Coder, Arc::new(Coder));
    registry.register(Role::Verifier, Arc::new(Verifier));
    registry
}

/// Which provider fills the local router slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LocalSlot {
    Candle,
    Ollama,
    Local,
}

/// Precedence for the local slot: candle (in-process) > ollama (HTTP) > offline Local.
fn choose_local_slot(candle_on: bool, ollama_on: bool) -> LocalSlot {
    if candle_on {
        LocalSlot::Candle
    } else if ollama_on {
        LocalSlot::Ollama
    } else {
        LocalSlot::Local
    }
}

/// Construct the local provider slot from the environment (shared by both router builders).
fn build_local_provider() -> Arc<dyn Provider> {
    // `OTTO_CANDLE` is honored only when the `candle` feature is compiled in.
    let candle_on = cfg!(feature = "candle") && std::env::var("OTTO_CANDLE").as_deref() == Ok("1");
    let ollama_on = std::env::var("OTTO_OLLAMA").as_deref() == Ok("1");
    if candle_on && ollama_on {
        eprintln!(
            "warning: both OTTO_CANDLE and OTTO_OLLAMA are set; using the in-process candle provider"
        );
    }
    match choose_local_slot(candle_on, ollama_on) {
        LocalSlot::Candle => {
            #[cfg(feature = "candle")]
            {
                build_candle_provider()
            }
            #[cfg(not(feature = "candle"))]
            {
                unreachable!("candle_on is false without the candle feature")
            }
        }
        LocalSlot::Ollama => {
            let model = std::env::var("OTTO_OLLAMA_MODEL")
                .unwrap_or_else(|_| DEFAULT_OLLAMA_MODEL.to_string());
            Arc::new(OllamaProvider::local_default(model))
        }
        LocalSlot::Local => Arc::new(LocalProvider::new()),
    }
}

/// Build the candle provider from `OTTO_CANDLE_*` env vars, falling back to the offline
/// `LocalProvider` (with a warning) if the model can't be loaded.
#[cfg(feature = "candle")]
fn build_candle_provider() -> Arc<dyn Provider> {
    use otto_providers::candle::{CandleProvider, GenConfig, resolve_model_source, select_device};
    let source = resolve_model_source(std::env::var("OTTO_CANDLE_MODEL").ok());
    match CandleProvider::new(source, GenConfig::from_env(), select_device()) {
        Ok(p) => Arc::new(p),
        Err(e) => {
            eprintln!("warning: candle provider unavailable ({e}); using offline LocalProvider");
            Arc::new(LocalProvider::new())
        }
    }
}

/// Which remote provider the router's single remote slot uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemoteChoice {
    Anthropic,
    OpenAi,
    Gemini,
    DeepSeek,
}

impl RemoteChoice {
    /// Stable id recorded in `session_config` and matching each provider's `Provider::id()`.
    fn id(self) -> &'static str {
        match self {
            RemoteChoice::Anthropic => "anthropic",
            RemoteChoice::OpenAi => "openai",
            RemoteChoice::Gemini => "gemini",
            RemoteChoice::DeepSeek => "deepseek",
        }
    }
}

/// Pure remote-slot selection for the default (non-pinned) path. Takes explicit inputs so it
/// is unit-testable without mutating process-global env (mirrors `capabilities_from_env`).
///
/// `selector` is the raw `OTTO_REMOTE_PROVIDER` value; the four bools are "this provider's key
/// is present and non-empty". A valid selector wins when its key is present; a selector whose
/// key is absent yields `None` (offline) rather than silently falling back to another
/// provider; an unknown selector is ignored and precedence applies:
/// Anthropic > OpenAI > Gemini > DeepSeek.
fn select_remote_from(
    selector: Option<&str>,
    anthropic: bool,
    openai: bool,
    gemini: bool,
    deepseek: bool,
) -> Option<RemoteChoice> {
    if let Some(sel) = selector {
        match sel.to_ascii_lowercase().as_str() {
            "anthropic" => {
                return present_or_warn(anthropic, RemoteChoice::Anthropic, "ANTHROPIC_API_KEY");
            }
            "openai" => return present_or_warn(openai, RemoteChoice::OpenAi, "OPENAI_API_KEY"),
            "gemini" => return present_or_warn(gemini, RemoteChoice::Gemini, "GEMINI_API_KEY"),
            "deepseek" => {
                return present_or_warn(deepseek, RemoteChoice::DeepSeek, "DEEPSEEK_API_KEY");
            }
            other => {
                eprintln!(
                    "warning: OTTO_REMOTE_PROVIDER='{other}' is not a known provider \
                     (anthropic|openai|gemini|deepseek); using key precedence instead"
                );
            }
        }
    }
    if anthropic {
        Some(RemoteChoice::Anthropic)
    } else if openai {
        Some(RemoteChoice::OpenAi)
    } else if gemini {
        Some(RemoteChoice::Gemini)
    } else if deepseek {
        Some(RemoteChoice::DeepSeek)
    } else {
        None
    }
}

/// Helper for `select_remote_from`: return the choice if its key is present, else warn and
/// select nothing (offline) — a named-but-unusable selector must not misroute to another key.
fn present_or_warn(present: bool, choice: RemoteChoice, key: &str) -> Option<RemoteChoice> {
    if present {
        Some(choice)
    } else {
        eprintln!(
            "warning: OTTO_REMOTE_PROVIDER='{}' but {key} is not set; \
             falling back to the offline/local router",
            choice.id()
        );
        None
    }
}

/// Pure model-id -> provider inference for the pinned path. Returns `None` for ids that do not
/// match a known provider prefix (the caller then uses the active remote, if any).
fn infer_remote(model: &str) -> Option<RemoteChoice> {
    if model.starts_with("gpt-")
        || model.starts_with("o1")
        || model.starts_with("o3")
        || model.starts_with("o4")
    {
        Some(RemoteChoice::OpenAi)
    } else if model.starts_with("gemini-") {
        Some(RemoteChoice::Gemini)
    } else if model.starts_with("claude-") {
        Some(RemoteChoice::Anthropic)
    } else if model.starts_with("deepseek-") {
        Some(RemoteChoice::DeepSeek)
    } else {
        None
    }
}

/// True when the given provider's API key is present and non-empty in the environment.
fn has_key(choice: RemoteChoice) -> bool {
    let var = match choice {
        RemoteChoice::Anthropic => "ANTHROPIC_API_KEY",
        RemoteChoice::OpenAi => "OPENAI_API_KEY",
        RemoteChoice::Gemini => "GEMINI_API_KEY",
        RemoteChoice::DeepSeek => "DEEPSEEK_API_KEY",
    };
    std::env::var(var).map(|k| !k.is_empty()).unwrap_or(false)
}

/// Env-reading wrapper over `select_remote_from` — the default-path remote selection.
fn select_remote() -> Option<RemoteChoice> {
    select_remote_from(
        std::env::var("OTTO_REMOTE_PROVIDER").ok().as_deref(),
        has_key(RemoteChoice::Anthropic),
        has_key(RemoteChoice::OpenAi),
        has_key(RemoteChoice::Gemini),
        has_key(RemoteChoice::DeepSeek),
    )
}

/// The effective default model for a provider (its `OTTO_<P>_MODEL` env var, else the constant).
fn default_model_for(choice: RemoteChoice) -> String {
    match choice {
        RemoteChoice::Anthropic => std::env::var("OTTO_ANTHROPIC_MODEL")
            .unwrap_or_else(|_| DEFAULT_ANTHROPIC_MODEL.to_string()),
        RemoteChoice::OpenAi => {
            std::env::var("OTTO_OPENAI_MODEL").unwrap_or_else(|_| DEFAULT_OPENAI_MODEL.to_string())
        }
        RemoteChoice::Gemini => {
            std::env::var("OTTO_GEMINI_MODEL").unwrap_or_else(|_| DEFAULT_GEMINI_MODEL.to_string())
        }
        RemoteChoice::DeepSeek => std::env::var("OTTO_DEEPSEEK_MODEL")
            .unwrap_or_else(|_| DEFAULT_DEEPSEEK_MODEL.to_string()),
    }
}

/// Resolve a provider's base URL from an optional `*_BASE_URL` override.
///
/// `Some(url)` is the base to use. `None` means an override was supplied but refused — the caller
/// must construct no provider, so the API key is never sent anywhere. Rejection is reported on
/// stderr naming the variable and the reason.
///
/// The override is taken as an argument rather than read from the environment here so the policy
/// is unit-testable without `set_var`; see the SAFETY note on the env-touching tests below.
fn resolve_base_url(
    override_value: Option<String>,
    default: &str,
    var_name: &str,
) -> Option<String> {
    // An exported-but-empty var means "unset" here, matching `has_key`'s treatment of an empty
    // API key. Compose files and `.env` templates routinely export `FOO=` for an unset value.
    let Some(value) = override_value.filter(|v| !v.is_empty()) else {
        return Some(default.to_string());
    };
    match otto_providers::validate_base_url(&value) {
        // The NORMALIZED form, not the operator's raw string: a downstream decision made by
        // inspecting the raw text (the provider client's proxy policy) would otherwise disagree
        // with what was validated here — `HTTP://127.0.0.1` and `http:/127.0.0.1` both validate
        // as loopback http but neither starts with the literal "http://".
        Ok(normalized) => Some(normalized),
        Err(e) => {
            // `e` is redacted to scheme://host:port — it carries neither the API key nor any
            // secret embedded in the rejected URL's userinfo or query.
            eprintln!(
                "warning: {var_name} was rejected: {e}; falling back to the offline/local router \
                 rather than sending the API key"
            );
            None
        }
    }
}

/// The `*_BASE_URL` override for a provider, or `None` for providers that have no override.
///
/// This is a table rather than a branch inside [`build_remote`] on purpose: adding an override for
/// a new provider is a one-line edit here, and it is then **impossible** to wire that provider up
/// without the validation in [`resolve_base_url`] running. Writing the `env::var` read inline in a
/// `build_remote` arm instead would compile, pass every test, and silently reintroduce the
/// cleartext-key bug this module exists to prevent.
fn base_url_var(choice: RemoteChoice) -> Option<&'static str> {
    match choice {
        RemoteChoice::OpenAi => Some("OPENAI_BASE_URL"),
        RemoteChoice::DeepSeek => Some("DEEPSEEK_BASE_URL"),
        // Fixed endpoints — `api_base_default()` only, no operator input reaches them.
        RemoteChoice::Anthropic | RemoteChoice::Gemini => None,
    }
}

/// Read a provider's `*_BASE_URL` override from the environment and validate it.
///
/// A non-UTF-8 value is treated as *invalid*, not as absent: silently falling back to the
/// production default would send the key to a host the operator did not intend.
fn env_base_url(choice: RemoteChoice, default: &str) -> Option<String> {
    let Some(var_name) = base_url_var(choice) else {
        return Some(default.to_string());
    };
    match std::env::var_os(var_name) {
        None => Some(default.to_string()),
        Some(raw) => match raw.into_string() {
            Ok(value) => resolve_base_url(Some(value), default, var_name),
            Err(_) => {
                eprintln!(
                    "warning: {var_name} is not valid UTF-8; falling back to the offline/local \
                     router rather than sending the API key to the default endpoint"
                );
                None
            }
        },
    }
}

/// Validate every `*_BASE_URL` override present in the environment, without building anything.
///
/// [`build_remote`] degrades to the offline router on a bad override, which is the right contract
/// for a library — but on `otto serve` that degrade is nearly invisible: the local slot is a
/// deterministic canned provider, so turns keep completing and *look* successful while a single
/// warning sits in the server's stderr. The binary, where a human set the variable, should refuse
/// to start instead. Call this from the CLI entrypoints before any router is built.
pub fn preflight_base_urls() -> anyhow::Result<()> {
    for choice in [
        RemoteChoice::Anthropic,
        RemoteChoice::OpenAi,
        RemoteChoice::Gemini,
        RemoteChoice::DeepSeek,
    ] {
        let Some(var_name) = base_url_var(choice) else {
            continue;
        };
        let Some(raw) = std::env::var_os(var_name) else {
            continue;
        };
        let value = raw.into_string().map_err(|_| {
            anyhow::anyhow!("{var_name} is set but is not valid UTF-8; unset it or fix the value")
        })?;
        if value.is_empty() {
            continue; // Explicitly empty means "unset"; the default endpoint is used.
        }
        otto_providers::validate_base_url(&value).map_err(|e| {
            anyhow::anyhow!(
                "{var_name} is invalid: {e}\n(checked at startup for every provider, whether or \
                 not it is the selected remote — unset it if you are not using that provider)"
            )
        })?;
    }
    Ok(())
}

/// Construct the remote provider for `choice`, pinned to `model`. Callers must have confirmed
/// the provider's key is present (`has_key`/`select_remote`); the key is read here.
///
/// Returns `None` when the provider's `*_BASE_URL` override fails validation, so a bad endpoint
/// degrades to the offline router instead of receiving the key.
fn build_remote(choice: RemoteChoice, model: String) -> Option<Arc<dyn Provider>> {
    match choice {
        RemoteChoice::Anthropic => Some(Arc::new(AnthropicProvider::new(
            AnthropicProvider::api_base_default(),
            std::env::var("ANTHROPIC_API_KEY").unwrap_or_default(),
            model,
        ))),
        RemoteChoice::OpenAi => {
            let base = env_base_url(choice, OpenAiProvider::api_base_default())?;
            Some(Arc::new(OpenAiProvider::new(
                base,
                std::env::var("OPENAI_API_KEY").unwrap_or_default(),
                model,
            )))
        }
        RemoteChoice::Gemini => Some(Arc::new(GeminiProvider::new(
            GeminiProvider::api_base_default(),
            std::env::var("GEMINI_API_KEY").unwrap_or_default(),
            model,
        ))),
        RemoteChoice::DeepSeek => {
            let base = env_base_url(choice, DeepSeekProvider::api_base_default())?;
            Some(Arc::new(DeepSeekProvider::new(
                base,
                std::env::var("DEEPSEEK_API_KEY").unwrap_or_default(),
                model,
            )))
        }
    }
}

/// Equivalent to [`build_router_with_model`]`(None)`; see that function for full behavior.
pub fn build_router() -> Box<dyn otto_engine_core::Router> {
    build_router_with_model(None)
}

/// Build a router, optionally pinning the remote slot to an explicit model id (from a
/// command/agent `model:` field).
///
/// - `model_override = None`: select a remote via [`select_remote`] (an `OTTO_REMOTE_PROVIDER`
///   selector, else key precedence Anthropic > OpenAI > Gemini > DeepSeek). Some ->
///   `BrainBlendRouter` over that provider at its default model; None -> the offline
///   `SingleProviderRouter`.
/// - `model_override = Some(m)`: infer the provider from `m`'s prefix ([`infer_remote`]); an
///   unrecognized prefix uses the active remote from [`select_remote`]. If the chosen
///   provider's key is present, build it pinned to `m` in a `PinnedModelRouter`; otherwise warn
///   and fall back to the offline `SingleProviderRouter` (keeping the default deterministic).
///
/// With no provider keys and no selector set, both branches yield
/// `SingleProviderRouter(LocalProvider)` — the byte-for-byte offline-deterministic default.
pub fn build_router_with_model(model_override: Option<&str>) -> Box<dyn otto_engine_core::Router> {
    let local = build_local_provider();

    match model_override {
        Some(model) => {
            // Known prefix is authoritative (its own key required); unknown prefix uses the
            // active remote. Either way the chosen provider's key must be present.
            let choice = infer_remote(model).or_else(select_remote);
            // `build_remote` returning None means the provider's *_BASE_URL override was
            // refused; it has already explained the specific reason on stderr. The generic
            // pinned-model warning below still fires, which is what we want here.
            match choice
                .filter(|c| has_key(*c))
                .and_then(|c| build_remote(c, model.to_string()))
            {
                Some(remote) => Box::new(PinnedModelRouter::new(local, remote)),
                None => {
                    eprintln!(
                        "warning: requested model '{model}' but no usable provider is available; \
                         falling back to the offline/local router"
                    );
                    Box::new(SingleProviderRouter::new(local))
                }
            }
        }
        None => match select_remote().and_then(|c| build_remote(c, default_model_for(c))) {
            Some(remote) => Box::new(BrainBlendRouter::new(local, remote)),
            // Either no remote was selected (the ordinary offline default, silent by design) or
            // its base URL was refused — in which case `resolve_base_url` already warned.
            None => Box::new(SingleProviderRouter::new(local)),
        },
    }
}

/// Build the tool registry with the default gate (ordinary writes auto-`Allow`ed).
pub fn build_tool_registry(workspace: Arc<dyn Workspace>, root: PathBuf) -> ToolRegistry {
    build_tool_registry_inner(workspace, root, false, None)
}

/// Build the tool registry in **approval mode**: ordinary `fs.write` is gated `Ask` (the
/// interactive approver applies it only on an explicit approval). The sensitive floor is
/// unchanged. Used by `serve --approve-edits`.
pub fn build_tool_registry_approving(workspace: Arc<dyn Workspace>, root: PathBuf) -> ToolRegistry {
    build_tool_registry_inner(workspace, root, true, None)
}

/// Build the tool registry with a `PolicyGate` applying `permissions` over the default gate,
/// optionally composed with approval mode. Used by the `otto run` spine (`approve_edits =
/// false`) and by `otto serve --approve-edits` (`approve_edits` reflects the flag) when
/// `.claude/settings.json` declares any permission rules. The PolicyGate always owns the bash
/// decision, so it pairs with a plain `DenyAsk` resolver; when `approve_edits` is true, an
/// `ApprovalModeGate` wraps the `PolicyGate` so an ordinary (rule-`Allow`ed) `fs.write` is
/// upgraded to `Ask` for interactive approval — a rule-driven `deny`/`ask` (and the sensitive
/// floor) still win.
pub fn build_tool_registry_with_permissions(
    workspace: Arc<dyn Workspace>,
    root: PathBuf,
    permissions: &PermissionRules,
    approve_edits: bool,
) -> ToolRegistry {
    build_tool_registry_inner(workspace, root, approve_edits, Some(permissions))
}

/// Always includes the sensitive-path-floor gate and the in-process fs tools. A sandboxed
/// `bash` tool is registered ONLY when an OS sandbox backend (bwrap/sandbox-exec) is
/// available; in that case the `Ask` verdict the gate gives `bash` is resolved by an
/// allow-list resolver (safe because the registered bash is OS-confined — and the
/// no-orphans-on-timeout guarantee holds because we only ever wire the `Os` policy, never
/// `None`). With no sandbox backend, `bash` is absent and the resolver denies all `Ask`
/// (fail-closed).
fn build_tool_registry_inner(
    workspace: Arc<dyn Workspace>,
    root: PathBuf,
    approve_edits: bool,
    permissions: Option<&PermissionRules>,
) -> ToolRegistry {
    let sandboxed = os_sandbox_available();
    let base_gate: Arc<dyn PermissionGate> = Arc::new(DefaultPermissionGate::new());
    // `approve_edits` wraps an `ApprovalModeGate` around whichever gate is otherwise chosen: an
    // ordinary (permitted) `fs.write` is upgraded to `Ask` for interactive approval, while a
    // `Deny` or an existing `Ask` (a permission rule, or the sensitive floor) pass through
    // unchanged. The orchestrator's edit-apply path treats any `Ask` on `fs.write` identically
    // regardless of which gate produced it, so this composes without special-casing there.
    let maybe_approve = |g: Arc<dyn PermissionGate>| -> Arc<dyn PermissionGate> {
        if approve_edits {
            Arc::new(ApprovalModeGate::new(g))
        } else {
            g
        }
    };

    // When permission rules exist, the PolicyGate owns every verdict (incl. bash), so it always
    // pairs with a plain DenyAsk.
    let (gate, ask): (Arc<dyn PermissionGate>, Arc<dyn AskResolver>) = match permissions {
        Some(rules) if !rules.is_empty() => {
            let policy_gate: Arc<dyn PermissionGate> =
                Arc::new(PolicyGate::new(base_gate, rules.clone(), sandboxed));
            (maybe_approve(policy_gate), Arc::new(DenyAsk))
        }
        _ => {
            // NB: the ask-resolver only ever auto-allows `bash`. An `Ask` on `fs.write` (approval
            // mode) is resolved by the orchestrator's `Approver`, never here — so writes can't
            // slip through.
            let ask: Arc<dyn AskResolver> = if sandboxed {
                Arc::new(AllowListAskResolver::new(vec!["bash".to_string()]))
            } else {
                Arc::new(DenyAsk)
            };
            (maybe_approve(base_gate), ask)
        }
    };

    let mut registry = ToolRegistry::new(gate, ask);
    // fs.read / fs.list need only the read-only view; fs.write holds the full workspace.
    let read_workspace: Arc<dyn WorkspaceRead> = workspace.clone();
    registry.register(Arc::new(FsReadTool::new(read_workspace.clone())));
    registry.register(Arc::new(FsWriteTool::new(Arc::clone(&workspace))));
    registry.register(Arc::new(FsListTool::new(read_workspace)));

    if sandboxed {
        registry.register(Arc::new(BashTool::new(
            root,
            SandboxPolicy::Os { allow_net: false },
        )));
    }

    registry
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

fn mcp_lsp_bin() -> String {
    std::env::var("OTTO_MCP_LSP_BIN").unwrap_or_else(|_| "mcp-lsp".to_string())
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
        build_tool_registry_with_permissions(
            tools_workspace,
            root.clone(),
            permissions,
            approve_edits,
        )
    } else if approve_edits {
        build_tool_registry_approving(tools_workspace, root.clone())
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

    // lsp: additive — absent (logged) if mcp-lsp can't start (its PATH gate finds no supported
    // language server: rust-analyzer / typescript-language-server / pyright-langserver / gopls).
    // No in-process fallback exists (there's no in-process LSP client), same category as grep/git.
    match mcp_connect_lsp(&mcp_lsp_bin(), &root).await {
        Ok((conn, mcp_tools)) => {
            for t in mcp_tools {
                registry.register(t);
            }
            conns.push(conn);
        }
        Err(e) => eprintln!("mcp-lsp unavailable ({e}); LSP tools disabled"),
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

/// The tool-registry composition every entrypoint shares (`otto run`, `otto run --command`,
/// `otto run --agent`, `otto serve`): the permission/approval gate from
/// `build_tools_preferring_mcp`, then skill registration via `register_skills`, then bundled plugin
/// MCP servers via `mcp_connect_plugin_server`, then hook-wrapping over all of them via
/// `register_hooks` (so hooks fire on plugin tools too). `approve_edits` is true only for
/// `otto serve --approve-edits`.
pub async fn build_composed_tools(
    ext: &otto_extensions::Extensions,
    tools_workspace: Arc<dyn Workspace>,
    root: PathBuf,
    approve_edits: bool,
) -> (ToolRegistry, Vec<McpConnection>) {
    let (mut tools, mut conns) = build_tools_preferring_mcp(
        tools_workspace,
        root.clone(),
        approve_edits,
        &ext.permissions,
    )
    .await;
    register_skills(&mut tools, &ext.skills);
    // Bundled plugin MCP servers register BEFORE register_hooks so hook-wrapping covers them too:
    // a `PreToolUse`/`PostToolUse` hook (matched via an `mcp__…` matcher or `*`) fires on plugin
    // tool calls. A server that won't spawn is logged and skipped — additive, never fatal.
    for spec in &ext.mcp_servers {
        match mcp_connect_plugin_server(spec).await {
            Ok((conn, mcp_tools)) => {
                for t in mcp_tools {
                    tools.register(t);
                }
                conns.push(conn);
            }
            Err(e) => eprintln!(
                "plugin mcp server {}:{} unavailable ({e}); skipping",
                spec.namespace, spec.server_key
            ),
        }
    }
    register_hooks(&mut tools, &ext.hooks, &root);
    (tools, conns)
}

/// Register the built-in `skill` tool when any skills were discovered. No-op otherwise, so a
/// workspace with no `.claude/skills/` leaves the spine's tool set byte-for-byte unchanged.
pub fn register_skills(
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
pub fn register_hooks(
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
        Arc::new(SandboxedHookExecutor::new(root.to_path_buf()));
    registry.wrap_each(|t| otto_extensions::HookedTool::wrap(t, hooks, exec.clone()));
}

/// Snapshot the provider-selection environment into JSON for a session's `config` column.
/// Mirrors the env that `build_router` reads (re-running remote selection so the record matches
/// the routing), so a stored session records which backends it was configured to use. Lives in
/// the wiring layer (not core) because it reads `OTTO_*` / provider API keys.
pub fn session_config() -> serde_json::Value {
    let ollama = std::env::var("OTTO_OLLAMA").as_deref() == Ok("1");
    let remote = select_remote();
    serde_json::json!({
        "ollama": ollama,
        "remote": remote.is_some(),
        // The resolved remote provider id ("anthropic"|"openai"|"gemini"|"deepseek") or "none".
        "remote_provider": remote.map(RemoteChoice::id).unwrap_or("none"),
        // Record the EFFECTIVE models (the build_router defaults when the env vars are unset),
        // so a restored session's config reflects the routing it actually used.
        "ollama_model": std::env::var("OTTO_OLLAMA_MODEL")
            .unwrap_or_else(|_| DEFAULT_OLLAMA_MODEL.to_string()),
        "remote_model": remote.map(default_model_for).unwrap_or_else(|| "none".to_string()),
    })
}

/// Pure capability derivation from raw env inputs. Kept separate from `build_capabilities`
/// so the mapping is unit-testable without mutating process-global env (which would race the
/// env-reading tests in this binary). `engine_remote` is always false here: `otto serve` is
/// the local engine; the promote path (sub-project F) provisions a separate remote engine
/// that computes its own manifest with `engine_remote = true`.
fn capabilities_from_env(
    otto_ollama: Option<&str>,
    remote_llm: bool,
    sandbox: bool,
) -> CapabilitiesManifest {
    CapabilitiesManifest {
        engine_remote: false,
        local_llm: otto_ollama == Some("1"),
        remote_llm,
        sandbox,
    }
}

/// Derive the running engine's capabilities from the environment `build_router` reads, plus
/// the OS sandbox probe. Lives in the wiring layer (not core) because it reads `OTTO_*` /
/// the provider API keys (via `select_remote`). Mirrors `session_config`'s predicates so a
/// session's recorded config and its reported capabilities stay consistent.
pub fn build_capabilities() -> CapabilitiesManifest {
    capabilities_from_env(
        std::env::var("OTTO_OLLAMA").ok().as_deref(),
        select_remote().is_some(),
        os_sandbox_available(),
    )
}

/// Build the persistent retriever for `root`, or `None` (logged) on any failure — retrieval is
/// an optimization, never a gate, so a missing cache dir or open error degrades to the lexical
/// fallback. The index DB lives in the OS cache dir, keyed by the canonical root so each repo
/// gets its own, reused across `otto run` invocations and serve restarts.
pub async fn build_retriever(
    root: &std::path::Path,
) -> Option<Arc<dyn otto_engine_core::Retriever>> {
    let canonical = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let Some(cache) = dirs::cache_dir() else {
        eprintln!("warning: no OS cache dir; retrieval index disabled (lexical fallback)");
        return None;
    };
    // Stable FNV-1a-64 over the canonical path bytes — unlike DefaultHasher, its output is
    // fixed across Rust toolchains, so a compiler upgrade does not orphan the on-disk index.
    let key = {
        use std::os::unix::ffi::OsStrExt;
        let mut h: u64 = 0xcbf29ce484222325;
        for b in canonical.as_os_str().as_bytes() {
            h ^= *b as u64;
            h = h.wrapping_mul(0x00000100000001B3);
        }
        h
    };
    let db = cache
        .join("otto")
        .join("index")
        .join(format!("{key:016x}.sqlite"));
    if let Some(parent) = db.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            eprintln!("warning: cannot create retrieval cache dir ({e}); lexical fallback");
            return None;
        }
    }
    match otto_retrieval::IndexedRetriever::open(canonical, db).await {
        Ok(r) => Some(Arc::new(r) as Arc<dyn otto_engine_core::Retriever>),
        Err(e) => {
            eprintln!("warning: retrieval index unavailable ({e}); lexical fallback");
            None
        }
    }
}

/// Run one turn for `goal` through an `EngineService` backed by `store`, returning the
/// sequenced events and the outcome. A thin wrapper: build the service, create a session,
/// run one prompt with a collecting sink.
pub async fn run_goal(
    goal: &str,
    store: Arc<dyn SessionStore>,
    router: Arc<dyn Router>,
    workspace: Arc<dyn Workspace>,
    tools: Arc<ToolRegistry>,
    retriever: Option<Arc<dyn otto_engine_core::Retriever>>,
) -> anyhow::Result<(Vec<Event>, TurnOutcome)> {
    let service = EngineService::new(
        store,
        Arc::new(build_default_registry()),
        router,
        workspace,
        tools,
    )
    .with_retriever(retriever);
    let owner = otto_protocol::UserId::local();
    let session = service
        .create_session(&owner, goal, &session_config())
        .await?;
    let mut sink = CollectingSink::default();
    let outcome = service.run_prompt(&owner, session, goal, &mut sink).await?;
    Ok((sink.events, outcome))
}

#[cfg(test)]
mod tests {
    use super::*;
    use otto_engine_core::RouteHints;
    use otto_engine_core::types::CompleteRequest;

    #[test]
    fn local_slot_precedence_candle_wins_over_ollama() {
        assert_eq!(choose_local_slot(true, true), LocalSlot::Candle);
        assert_eq!(choose_local_slot(true, false), LocalSlot::Candle);
        assert_eq!(choose_local_slot(false, true), LocalSlot::Ollama);
        assert_eq!(choose_local_slot(false, false), LocalSlot::Local);
    }

    // The three `resolve_base_url` tests below deliberately pass the override in as an argument
    // rather than setting an env var. That keeps the SAFETY contract on
    // `default_build_router_is_offline_and_deterministic` true as written: no test in this binary
    // SETS a provider-selection var, so there is no destructive race.

    #[test]
    fn absent_base_url_override_uses_the_provider_default() {
        assert_eq!(
            resolve_base_url(None, "https://api.openai.com", "OPENAI_BASE_URL"),
            Some("https://api.openai.com".to_string())
        );
    }

    #[test]
    fn empty_base_url_override_is_treated_as_unset() {
        // `FOO=` is how compose files and .env templates spell "not set". Treating it as an
        // invalid override would make the provider silently vanish; `has_key` already treats an
        // empty API key as absent, so this matches the file's own convention.
        assert_eq!(
            resolve_base_url(
                Some(String::new()),
                "https://api.openai.com",
                "OPENAI_BASE_URL"
            ),
            Some("https://api.openai.com".to_string())
        );
    }

    #[test]
    fn only_providers_with_an_override_consult_the_environment() {
        // The table is what makes validation impossible to skip: a provider with no entry can
        // never read an operator-supplied base, and one with an entry always goes through
        // `resolve_base_url`. If a future provider gains an override, it belongs here.
        assert_eq!(base_url_var(RemoteChoice::OpenAi), Some("OPENAI_BASE_URL"));
        assert_eq!(
            base_url_var(RemoteChoice::DeepSeek),
            Some("DEEPSEEK_BASE_URL")
        );
        assert_eq!(base_url_var(RemoteChoice::Anthropic), None);
        assert_eq!(base_url_var(RemoteChoice::Gemini), None);
    }

    #[test]
    fn valid_base_url_override_is_accepted_and_normalized() {
        // https anywhere, and plain http to loopback (the wiremock shape), are both honored. The
        // value returned is the parser's NORMALIZED form, not the raw input — that is what keeps
        // the provider client's proxy decision in sync with what was validated. Normalization may
        // append a trailing `/`, which `join_url` trims when composing the endpoint.
        for candidate in [
            "https://gateway.internal.example.com/v1",
            "http://127.0.0.1:8080",
            "http://localhost:1234",
        ] {
            let resolved = resolve_base_url(
                Some(candidate.to_string()),
                "https://api.openai.com",
                "OPENAI_BASE_URL",
            )
            .unwrap_or_else(|| panic!("expected {candidate} to be accepted"));
            assert_eq!(resolved.trim_end_matches('/'), candidate);
        }
    }

    #[test]
    fn accepted_override_always_carries_an_unambiguous_scheme() {
        // Spellings that validate as loopback http but do NOT start with the literal "http://".
        // If the raw string were passed through, the provider client would misread the scheme and
        // skip no_proxy(), shipping the cleartext request (key included) to an HTTP_PROXY.
        for raw in [
            "HTTP://127.0.0.1:9",
            "http:/127.0.0.1:9",
            " http://127.0.0.1:9",
        ] {
            let resolved = resolve_base_url(
                Some(raw.to_string()),
                "https://api.openai.com",
                "OPENAI_BASE_URL",
            )
            .unwrap_or_else(|| panic!("expected {raw} to be accepted"));
            assert!(
                resolved.starts_with("http://"),
                "{raw} resolved to {resolved}, which hides the scheme from the client"
            );
        }
    }

    #[test]
    fn rejected_base_url_override_yields_no_provider() {
        // Each of these would otherwise have received the API key as a Bearer header. `None`
        // means build_remote constructs nothing and the caller falls back to the offline router.
        for candidate in [
            "http://api.openai.com",
            "http://evil.example.com",
            "http://localhost.evil.com",
            "http://169.254.169.254",
            "ftp://host",
            "not a url",
            // Note: "" is deliberately NOT here — an empty override means "unset"; see
            // `empty_base_url_override_is_treated_as_unset`.
        ] {
            assert_eq!(
                resolve_base_url(
                    Some(candidate.to_string()),
                    "https://api.deepseek.com",
                    "DEEPSEEK_BASE_URL"
                ),
                None,
                "expected {candidate:?} to be rejected"
            );
        }
    }

    #[tokio::test]
    async fn default_build_router_is_offline_and_deterministic() {
        // Ensure the env that would select real backends is absent for this test.
        // SAFETY: there are exactly two tests in this lib's test binary that touch these
        // provider-selection vars: this one and
        // `model_override_without_key_is_offline_and_deterministic`. Both only ever
        // REMOVE these vars (removing an absent var is idempotent), so ordering is
        // irrelevant and they cannot race destructively. Do not add a test here that
        // SETS these vars without revisiting this comment.
        unsafe {
            std::env::remove_var("OTTO_OLLAMA");
            std::env::remove_var("ANTHROPIC_API_KEY");
            std::env::remove_var("OPENAI_API_KEY");
            std::env::remove_var("GEMINI_API_KEY");
            std::env::remove_var("DEEPSEEK_API_KEY");
            std::env::remove_var("OTTO_REMOTE_PROVIDER");
        }
        let router = build_router();
        let a = router
            .complete(
                CompleteRequest {
                    prompt: "ping".into(),
                },
                RouteHints::default(),
            )
            .await
            .unwrap();
        let b = router
            .complete(
                CompleteRequest {
                    prompt: "ping".into(),
                },
                RouteHints::default(),
            )
            .await
            .unwrap();
        assert_eq!(a, b);
        assert!(a.text.contains("ping"));
    }

    #[tokio::test]
    async fn model_override_without_key_is_offline_and_deterministic() {
        // SAFETY: the only other test touching these vars is the sibling
        // `default_build_router_is_offline_and_deterministic`; both only ever REMOVE (never set)
        // these vars, and removing an absent var is idempotent, so ordering is irrelevant.
        unsafe {
            std::env::remove_var("OTTO_OLLAMA");
            std::env::remove_var("ANTHROPIC_API_KEY");
            std::env::remove_var("OPENAI_API_KEY");
            std::env::remove_var("GEMINI_API_KEY");
            std::env::remove_var("DEEPSEEK_API_KEY");
            std::env::remove_var("OTTO_REMOTE_PROVIDER");
        }
        // Naming a model with no provider key must NOT change routing: it falls back to the
        // offline local router (a warning is printed to stderr, not asserted here).
        let router = build_router_with_model(Some("claude-opus-4-8"));
        let a = router
            .complete(
                CompleteRequest {
                    prompt: "ping".into(),
                },
                RouteHints::default(),
            )
            .await
            .unwrap();
        let b = router
            .complete(
                CompleteRequest {
                    prompt: "ping".into(),
                },
                RouteHints::default(),
            )
            .await
            .unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn select_remote_from_precedence_when_no_selector() {
        // Precedence: Anthropic > OpenAi > Gemini > DeepSeek among present keys.
        assert_eq!(
            select_remote_from(None, true, true, true, true),
            Some(RemoteChoice::Anthropic)
        );
        assert_eq!(
            select_remote_from(None, false, true, true, true),
            Some(RemoteChoice::OpenAi)
        );
        assert_eq!(
            select_remote_from(None, false, false, true, true),
            Some(RemoteChoice::Gemini)
        );
        assert_eq!(
            select_remote_from(None, false, false, false, true),
            Some(RemoteChoice::DeepSeek)
        );
        assert_eq!(select_remote_from(None, false, false, false, false), None);
    }

    #[test]
    fn select_remote_from_selector_wins_when_its_key_present() {
        // A valid selector overrides precedence even when a higher-precedence key exists.
        assert_eq!(
            select_remote_from(Some("openai"), true, true, true, true),
            Some(RemoteChoice::OpenAi)
        );
        assert_eq!(
            select_remote_from(Some("gemini"), true, false, true, true),
            Some(RemoteChoice::Gemini)
        );
        assert_eq!(
            select_remote_from(Some("deepseek"), true, true, true, true),
            Some(RemoteChoice::DeepSeek)
        );
        // Case-insensitive.
        assert_eq!(
            select_remote_from(Some("OpenAI"), false, true, false, false),
            Some(RemoteChoice::OpenAi)
        );
        assert_eq!(
            select_remote_from(Some("DeepSeek"), true, true, true, true),
            Some(RemoteChoice::DeepSeek)
        );
    }

    #[test]
    fn select_remote_from_selector_without_key_is_none() {
        // Selector names a provider whose key is absent -> None (offline), NOT a fallback to
        // another provider's key.
        assert_eq!(
            select_remote_from(Some("openai"), true, false, true, false),
            None
        );
        assert_eq!(
            select_remote_from(Some("gemini"), true, true, false, false),
            None
        );
        assert_eq!(
            select_remote_from(Some("deepseek"), true, true, true, false),
            None
        );
    }

    #[test]
    fn select_remote_from_unknown_selector_falls_through_to_precedence() {
        assert_eq!(
            select_remote_from(Some("bogus"), true, false, false, false),
            Some(RemoteChoice::Anthropic)
        );
        assert_eq!(
            select_remote_from(Some("bogus"), false, false, false, false),
            None
        );
    }

    #[test]
    fn infer_remote_maps_model_id_prefixes() {
        assert_eq!(infer_remote("gpt-4o"), Some(RemoteChoice::OpenAi));
        assert_eq!(infer_remote("gpt-4o-mini"), Some(RemoteChoice::OpenAi));
        assert_eq!(infer_remote("o1-preview"), Some(RemoteChoice::OpenAi));
        assert_eq!(infer_remote("o3-mini"), Some(RemoteChoice::OpenAi));
        assert_eq!(infer_remote("gemini-2.5-pro"), Some(RemoteChoice::Gemini));
        assert_eq!(
            infer_remote("claude-opus-4-8"),
            Some(RemoteChoice::Anthropic)
        );
        assert_eq!(
            infer_remote("deepseek-v4-flash"),
            Some(RemoteChoice::DeepSeek)
        );
        assert_eq!(
            infer_remote("deepseek-reasoner"),
            Some(RemoteChoice::DeepSeek)
        );
        assert_eq!(infer_remote("llama3.2"), None);
        assert_eq!(infer_remote("mistral-large"), None);
    }

    #[test]
    fn capabilities_from_env_maps_flags() {
        // Pure mapping — takes raw inputs, touches no process-global env, so it does NOT race
        // the env-reading router test in this same binary.
        // Nothing set → fully offline, local engine, no sandbox.
        assert_eq!(
            capabilities_from_env(None, false, false),
            CapabilitiesManifest {
                engine_remote: false,
                local_llm: false,
                remote_llm: false,
                sandbox: false,
            }
        );
        // OTTO_OLLAMA must equal exactly "1" to count as a local LLM.
        assert!(capabilities_from_env(Some("1"), false, false).local_llm);
        assert!(!capabilities_from_env(Some("0"), false, false).local_llm);
        // remote_llm now reflects "a remote provider is selectable" (any of the four keys /
        // a valid selector), computed by the caller via select_remote().is_some().
        assert!(capabilities_from_env(None, true, false).remote_llm);
        assert!(!capabilities_from_env(None, false, false).remote_llm);
        // sandbox passes through unchanged.
        assert!(capabilities_from_env(None, false, true).sandbox);
    }

    #[tokio::test]
    async fn registry_with_permissions_denies_matched_write() {
        use otto_extensions::parse_permissions;
        use otto_workspace::LocalWorkspace;
        use serde_json::json;
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        let ws: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(dir.path().to_path_buf()));
        let rules = parse_permissions(r#"{ "permissions": { "deny": ["Write(dist/**)"] } }"#);
        let reg = build_tool_registry_with_permissions(ws, dir.path().to_path_buf(), &rules, false);

        // A write to a denied path is rejected by the gate before dispatch.
        let err = reg
            .call("fs.write", json!({"path": "dist/x.txt", "contents": "hi"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("denied by permission gate"));

        // An unmatched write is permitted.
        assert!(
            reg.call("fs.write", json!({"path": "src/x.txt", "contents": "hi"}))
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn registry_with_permissions_and_approval_upgrades_ordinary_write_to_ask() {
        use otto_engine_core::tool::Decision;
        use otto_extensions::parse_permissions;
        use otto_workspace::LocalWorkspace;
        use serde_json::json;
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        let ws: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(dir.path().to_path_buf()));
        let rules = parse_permissions(r#"{ "permissions": { "deny": ["Write(dist/**)"] } }"#);
        let reg = build_tool_registry_with_permissions(ws, dir.path().to_path_buf(), &rules, true);

        // An ordinary write (no matching rule) is upgraded from the PolicyGate's Allow to Ask
        // for interactive approval, not silently applied.
        assert_eq!(
            reg.check("fs.write", &json!({"path": "src/x.txt"})),
            Decision::Ask
        );
        // A rule-driven deny still wins over approval mode.
        assert_eq!(
            reg.check("fs.write", &json!({"path": "dist/x.txt"})),
            Decision::Deny
        );
        // The sensitive-path floor still wins over everything.
        assert_eq!(
            reg.check("fs.write", &json!({"path": ".env"})),
            Decision::Deny
        );
    }

    #[tokio::test]
    async fn registry_with_permissions_and_approval_preserves_rule_driven_ask() {
        use otto_engine_core::tool::Decision;
        use otto_extensions::parse_permissions;
        use otto_workspace::LocalWorkspace;
        use serde_json::json;
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        let ws: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(dir.path().to_path_buf()));
        let rules = parse_permissions(r#"{ "permissions": { "ask": ["Write(secrets/**)"] } }"#);
        let reg = build_tool_registry_with_permissions(ws, dir.path().to_path_buf(), &rules, true);

        // A rule-driven `ask` on write is unaffected by the ApprovalModeGate wrap (it only
        // upgrades Allow, never re-classifies an existing Ask) — it still reaches interactive
        // approval, same as an ordinary write would.
        assert_eq!(
            reg.check("fs.write", &json!({"path": "secrets/x.txt"})),
            Decision::Ask
        );
    }

    #[tokio::test]
    async fn build_composed_tools_wraps_hooks_around_permission_and_approval_gate() {
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
            super::build_composed_tools(&ext, ws, proj.path().to_path_buf(), true).await;

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

    #[tokio::test]
    async fn build_composed_tools_enforces_hooks_on_the_plain_gate_branch() {
        use otto_workspace::LocalWorkspace;

        if !otto_tools::os_sandbox_available() {
            eprintln!("skipping serve hooks plain-branch test: no OS sandbox backend");
            return;
        }
        let proj = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        std::fs::write(proj.path().join("target.txt"), "hi").unwrap();
        let claude = proj.path().join(".claude");
        std::fs::create_dir_all(&claude).unwrap();
        std::fs::write(
            claude.join("settings.json"),
            r#"{"hooks": { "PreToolUse": [
                {"matcher": "fs.read", "hooks": [{"type": "command", "command": "exit 2"}]}
            ] }}"#,
        )
        .unwrap();

        let ext = otto_extensions::discover(proj.path(), home.path());
        assert!(ext.permissions.is_empty());
        assert!(!ext.hooks.is_empty());

        // No permission rules and approve_edits=false: build_tools_preferring_mcp takes its
        // plain build_tool_registry branch, not PolicyGate/ApprovalModeGate.
        let ws: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(proj.path().to_path_buf()));
        let (tools, _conns) =
            super::build_composed_tools(&ext, ws, proj.path().to_path_buf(), false).await;

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
    async fn build_composed_tools_matches_direct_call_when_nothing_is_configured() {
        use otto_workspace::LocalWorkspace;

        let proj = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        std::fs::write(proj.path().join("target.txt"), "hi").unwrap();

        let ext = otto_extensions::discover(proj.path(), home.path());
        assert!(ext.permissions.is_empty());
        assert!(ext.hooks.is_empty());

        let ws: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(proj.path().to_path_buf()));
        let (tools, _conns) =
            super::build_composed_tools(&ext, ws, proj.path().to_path_buf(), false).await;

        // With no settings.json at all, build_composed_tools must behave exactly like calling
        // build_tools_preferring_mcp directly — register_hooks is a no-op with no hooks.
        let out = tools
            .call("fs.read", serde_json::json!({ "path": "target.txt" }))
            .await
            .unwrap();
        assert!(
            out.to_string().contains("hi"),
            "expected fs.read to return content, got: {out}"
        );
    }

    #[tokio::test]
    async fn build_composed_tools_registers_skill_tool_when_present() {
        use otto_workspace::LocalWorkspace;

        let proj = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let skill_dir = proj.path().join(".claude").join("skills").join("greeter");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: greeter\ndescription: greets\n---\nSay hi.\n",
        )
        .unwrap();

        let ext = otto_extensions::discover(proj.path(), home.path());
        assert!(!ext.skills.is_empty());

        let ws: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(proj.path().to_path_buf()));
        let (tools, _conns) =
            super::build_composed_tools(&ext, ws, proj.path().to_path_buf(), false).await;

        assert!(
            tools.tool_names().iter().any(|n| n == "skill"),
            "expected the `skill` tool to be registered, got: {:?}",
            tools.tool_names()
        );
    }

    #[tokio::test]
    async fn build_composed_tools_connects_and_registers_a_plugin_mcp_server() {
        use otto_extensions::{Extensions, PluginMcpServer};
        use otto_workspace::LocalWorkspace;

        // Use the real, already-built mcp-fs binary as a stand-in "plugin" MCP server — a real
        // stdio server, so this proves the actual connect-and-register path, not a mock.
        let bin = escargot::CargoBuild::new()
            .package("otto-mcp-fs")
            .bin("mcp-fs")
            .run()
            .expect("build mcp-fs")
            .path()
            .to_path_buf();

        let proj = tempfile::tempdir().unwrap();
        std::fs::write(proj.path().join("target.txt"), "hi").unwrap();

        let mut ext = Extensions::default();
        ext.mcp_servers.push(PluginMcpServer {
            namespace: "testplugin".to_string(),
            server_key: "fs".to_string(),
            command: bin.to_string_lossy().into_owned(),
            args: vec![proj.path().to_string_lossy().into_owned()],
            env: Default::default(),
            cwd: None,
        });

        let ws: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(proj.path().to_path_buf()));
        let (tools, conns) =
            super::build_composed_tools(&ext, ws, proj.path().to_path_buf(), false).await;

        assert!(
            tools
                .tool_names()
                .iter()
                .any(|n| n == "plugin__testplugin__fs__fs.read"),
            "expected the namespaced plugin tool to be registered, got: {:?}",
            tools.tool_names()
        );
        // The connection must be retained in the returned Vec — otherwise the caller would drop
        // it and kill the child process the instant build_composed_tools returns.
        assert!(!conns.is_empty());

        // Registered-by-name isn't enough on its own — prove the tool actually round-trips
        // through the spawned server (catches a namespacing/routing bug that a name-only
        // assertion would miss, e.g. the request reaching the server under the wrong tool name).
        let out = tools
            .call(
                "plugin__testplugin__fs__fs.read",
                serde_json::json!({ "path": "target.txt" }),
            )
            .await
            .unwrap();
        assert!(
            out.to_string().contains("hi"),
            "expected the plugin tool call to return the file content, got: {out}"
        );
    }

    #[tokio::test]
    async fn build_composed_tools_skips_an_unreachable_plugin_mcp_server() {
        use otto_extensions::{Extensions, PluginMcpServer};
        use otto_workspace::LocalWorkspace;

        let proj = tempfile::tempdir().unwrap();
        std::fs::write(proj.path().join("target.txt"), "hi").unwrap();

        let mut ext = Extensions::default();
        ext.mcp_servers.push(PluginMcpServer {
            namespace: "testplugin".to_string(),
            server_key: "bogus".to_string(),
            command: "definitely-not-a-real-binary-xyz".to_string(),
            args: vec![],
            env: Default::default(),
            cwd: None,
        });

        let ws: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(proj.path().to_path_buf()));
        let (tools, conns) =
            super::build_composed_tools(&ext, ws, proj.path().to_path_buf(), false).await;

        // An unreachable plugin server is logged and skipped, never fatal — matches cmd_run.
        assert!(
            !tools.tool_names().iter().any(|n| n.starts_with("plugin__")),
            "expected no plugin tools to be registered, got: {:?}",
            tools.tool_names()
        );
        assert!(conns.is_empty());
    }

    #[tokio::test]
    async fn build_composed_tools_hook_wraps_plugin_mcp_tools() {
        use otto_extensions::PluginMcpServer;
        use otto_workspace::LocalWorkspace;

        if !otto_tools::os_sandbox_available() {
            eprintln!(
                "skipping plugin-hook-ordering test: no OS sandbox backend, hooks would be skipped"
            );
            return;
        }

        let bin = escargot::CargoBuild::new()
            .package("otto-mcp-fs")
            .bin("mcp-fs")
            .run()
            .expect("build mcp-fs")
            .path()
            .to_path_buf();

        let proj = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        std::fs::write(proj.path().join("target.txt"), "hi").unwrap();
        let claude = proj.path().join(".claude");
        std::fs::create_dir_all(&claude).unwrap();
        std::fs::write(
            claude.join("settings.json"),
            r#"{"hooks": { "PreToolUse": [
                {"matcher": "*", "hooks": [{"type": "command", "command": "exit 2"}]}
            ] }}"#,
        )
        .unwrap();

        // A "*" PreToolUse hook blocks every tool in the registry when register_hooks wraps it.
        // Both fs.read and the plugin tool register before the wrap now, so both must be blocked.
        let mut ext = otto_extensions::discover(proj.path(), home.path());
        assert!(!ext.hooks.is_empty());
        ext.mcp_servers.push(PluginMcpServer {
            namespace: "testplugin".to_string(),
            server_key: "fs".to_string(),
            command: bin.to_string_lossy().into_owned(),
            args: vec![proj.path().to_string_lossy().into_owned()],
            env: Default::default(),
            cwd: None,
        });

        let ws: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(proj.path().to_path_buf()));
        let (tools, _conns) =
            super::build_composed_tools(&ext, ws, proj.path().to_path_buf(), false).await;

        // fs.read was registered before the hook wrap — the "*" hook blocks it.
        let blocked = tools
            .call("fs.read", serde_json::json!({ "path": "target.txt" }))
            .await
            .unwrap_err();
        assert!(
            blocked.to_string().contains("blocked by PreToolUse hook"),
            "got: {blocked}"
        );

        // The plugin tool now registers BEFORE the hook wrap — the same "*" hook must block it too.
        let plugin_blocked = tools
            .call(
                "plugin__testplugin__fs__fs.read",
                serde_json::json!({ "path": "target.txt" }),
            )
            .await
            .unwrap_err();
        assert!(
            plugin_blocked
                .to_string()
                .contains("blocked by PreToolUse hook"),
            "expected the wrapped plugin tool to be blocked, got: {plugin_blocked}"
        );
    }

    #[tokio::test]
    async fn build_composed_tools_mcp_matcher_hook_fires_on_plugin_tool_only() {
        use otto_extensions::PluginMcpServer;
        use otto_workspace::LocalWorkspace;

        if !otto_tools::os_sandbox_available() {
            eprintln!(
                "skipping mcp-matcher hook test: no OS sandbox backend, hooks would be skipped"
            );
            return;
        }

        let bin = escargot::CargoBuild::new()
            .package("otto-mcp-fs")
            .bin("mcp-fs")
            .run()
            .expect("build mcp-fs")
            .path()
            .to_path_buf();

        let proj = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        std::fs::write(proj.path().join("target.txt"), "hi").unwrap();
        let claude = proj.path().join(".claude");
        std::fs::create_dir_all(&claude).unwrap();
        // Matcher targets ONLY this plugin's MCP tools — fs.read must be untouched.
        std::fs::write(
            claude.join("settings.json"),
            r#"{"hooks": { "PreToolUse": [
                {"matcher": "mcp__testplugin", "hooks": [{"type": "command", "command": "exit 2"}]}
            ] }}"#,
        )
        .unwrap();

        let mut ext = otto_extensions::discover(proj.path(), home.path());
        assert!(!ext.hooks.is_empty());
        ext.mcp_servers.push(PluginMcpServer {
            namespace: "testplugin".to_string(),
            server_key: "fs".to_string(),
            command: bin.to_string_lossy().into_owned(),
            args: vec![proj.path().to_string_lossy().into_owned()],
            env: Default::default(),
            cwd: None,
        });

        let ws: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(proj.path().to_path_buf()));
        let (tools, _conns) =
            super::build_composed_tools(&ext, ws, proj.path().to_path_buf(), false).await;

        // fs.read is NOT selected by the mcp__testplugin matcher → it runs.
        let ok = tools
            .call("fs.read", serde_json::json!({ "path": "target.txt" }))
            .await
            .unwrap();
        assert!(
            ok.to_string().contains("hi"),
            "fs.read should not be blocked, got: {ok}"
        );

        // The plugin tool IS selected → blocked.
        let blocked = tools
            .call(
                "plugin__testplugin__fs__fs.read",
                serde_json::json!({ "path": "target.txt" }),
            )
            .await
            .unwrap_err();
        assert!(
            blocked.to_string().contains("blocked by PreToolUse hook"),
            "expected the plugin tool to be blocked by the mcp__ matcher, got: {blocked}"
        );
    }
}
