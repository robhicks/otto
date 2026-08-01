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
    export_bundle, promote,
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

/// Construct the remote provider for `choice`, pinned to `model`. Callers must have confirmed
/// the provider's key is present (`has_key`/`select_remote`); the key is read here.
fn build_remote(choice: RemoteChoice, model: String) -> Arc<dyn Provider> {
    match choice {
        RemoteChoice::Anthropic => Arc::new(AnthropicProvider::new(
            AnthropicProvider::api_base_default(),
            std::env::var("ANTHROPIC_API_KEY").unwrap_or_default(),
            model,
        )),
        RemoteChoice::OpenAi => {
            let base = std::env::var("OPENAI_BASE_URL")
                .unwrap_or_else(|_| OpenAiProvider::api_base_default().to_string());
            Arc::new(OpenAiProvider::new(
                base,
                std::env::var("OPENAI_API_KEY").unwrap_or_default(),
                model,
            ))
        }
        RemoteChoice::Gemini => Arc::new(GeminiProvider::new(
            GeminiProvider::api_base_default(),
            std::env::var("GEMINI_API_KEY").unwrap_or_default(),
            model,
        )),
        RemoteChoice::DeepSeek => {
            let base = std::env::var("DEEPSEEK_BASE_URL")
                .unwrap_or_else(|_| DeepSeekProvider::api_base_default().to_string());
            Arc::new(DeepSeekProvider::new(
                base,
                std::env::var("DEEPSEEK_API_KEY").unwrap_or_default(),
                model,
            ))
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
            match choice.filter(|c| has_key(*c)) {
                Some(c) => {
                    let remote = build_remote(c, model.to_string());
                    Box::new(PinnedModelRouter::new(local, remote))
                }
                None => {
                    eprintln!(
                        "warning: requested model '{model}' but no usable provider key is set; \
                         falling back to the offline/local router"
                    );
                    Box::new(SingleProviderRouter::new(local))
                }
            }
        }
        None => match select_remote() {
            Some(c) => {
                let remote = build_remote(c, default_model_for(c));
                Box::new(BrainBlendRouter::new(local, remote))
            }
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
    let session = service.create_session(goal, &session_config()).await?;
    let mut sink = CollectingSink::default();
    let outcome = service.run_prompt(session, goal, &mut sink).await?;
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
}
