//! Engine wiring: assemble the default agent registry and run a turn end-to-end.

use std::path::PathBuf;
use std::sync::Arc;

use otto_agents::{Coder, ContextFinder, Planner, Verifier};
use otto_engine_core::tool::{
    AllowListAskResolver, AskResolver, DenyAsk, PermissionGate, ToolRegistry,
};
use otto_engine_core::traits::{Provider, Workspace, WorkspaceRead};
use otto_engine_core::{AgentRegistry, Router, TurnOutcome};
use otto_persistence::SessionStore;
use otto_protocol::{CapabilitiesManifest, Event, Role};
use otto_providers::{AnthropicProvider, LocalProvider, OllamaProvider};
use otto_router::{BrainBlendRouter, SingleProviderRouter};
use otto_tools::{
    BashTool, DefaultPermissionGate, FsListTool, FsReadTool, FsWriteTool, SandboxPolicy,
    os_sandbox_available,
};

mod approval;
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
    connect_plugin_server as mcp_connect_plugin_server,
};
pub use otto_remote::{
    MicrovmConfig, MicrovmTarget, PromoteBundle, PromoteConfig, PromoteMode, ProvisionedMachine,
    Provisioner, RemoteHandle, RemoteTarget, UnsupportedProvisioner, VpsTarget, export_bundle,
    promote,
};
pub use policy_gate::PolicyGate;
pub use serve::{
    app as serve_app, app_with_base as serve_app_with_base, resolve_tls_paths, run as serve_run,
};
pub use service::{AcceptError, CollectingSink, EngineService, EventSink, TurnControls};

/// Default model ids when the corresponding `OTTO_*_MODEL` env var is unset. Referenced by
/// both `build_router` (selection) and `session_config` (recording the effective model).
const DEFAULT_OLLAMA_MODEL: &str = "llama3.2";
const DEFAULT_ANTHROPIC_MODEL: &str = "claude-haiku-4-5";

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

/// Build a router from environment configuration.
///
/// - Local slot: `OllamaProvider` if `OTTO_OLLAMA=1` (model from `OTTO_OLLAMA_MODEL`,
///   default `llama3.2`), otherwise the deterministic `LocalProvider`.
/// - Remote slot: `AnthropicProvider` if `ANTHROPIC_API_KEY` is set (model from
///   `OTTO_ANTHROPIC_MODEL`, default `claude-haiku-4-5`), otherwise the local slot is
///   reused so routing still works with one real backend.
///
/// With no env vars set, both slots are the deterministic `LocalProvider`, so the engine
/// runs fully offline and deterministically — the default for tests and first-run.
pub fn build_router() -> Box<dyn otto_engine_core::Router> {
    let local: Arc<dyn Provider> = if std::env::var("OTTO_OLLAMA").as_deref() == Ok("1") {
        let model =
            std::env::var("OTTO_OLLAMA_MODEL").unwrap_or_else(|_| DEFAULT_OLLAMA_MODEL.to_string());
        Arc::new(OllamaProvider::local_default(model))
    } else {
        Arc::new(LocalProvider::new())
    };

    match std::env::var("ANTHROPIC_API_KEY") {
        Ok(key) if !key.is_empty() => {
            let model = std::env::var("OTTO_ANTHROPIC_MODEL")
                .unwrap_or_else(|_| DEFAULT_ANTHROPIC_MODEL.to_string());
            let remote: Arc<dyn Provider> = Arc::new(AnthropicProvider::new(
                AnthropicProvider::api_base_default(),
                key,
                model,
            ));
            Box::new(BrainBlendRouter::new(local, remote))
        }
        _ => Box::new(SingleProviderRouter::new(local)),
    }
}

/// Build the tool registry with the default gate (ordinary writes auto-`Allow`ed).
pub fn build_tool_registry(workspace: Arc<dyn Workspace>, root: PathBuf) -> ToolRegistry {
    build_tool_registry_inner(workspace, root, false)
}

/// Build the tool registry in **approval mode**: ordinary `fs.write` is gated `Ask` (the
/// interactive approver applies it only on an explicit approval). The sensitive floor is
/// unchanged. Used by `serve --approve-edits`.
pub fn build_tool_registry_approving(workspace: Arc<dyn Workspace>, root: PathBuf) -> ToolRegistry {
    build_tool_registry_inner(workspace, root, true)
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
) -> ToolRegistry {
    let sandboxed = os_sandbox_available();
    // NB: the ask-resolver only ever auto-allows `bash`. An `Ask` on `fs.write` (approval mode)
    // is resolved by the orchestrator's `Approver`, never here — so writes can't slip through.
    let ask: Arc<dyn AskResolver> = if sandboxed {
        Arc::new(AllowListAskResolver::new(vec!["bash".to_string()]))
    } else {
        Arc::new(DenyAsk)
    };

    let base_gate: Arc<dyn PermissionGate> = Arc::new(DefaultPermissionGate::new());
    let gate: Arc<dyn PermissionGate> = if approve_edits {
        Arc::new(ApprovalModeGate::new(base_gate))
    } else {
        base_gate
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
/// Mirrors the env that `build_router` reads (without re-running provider selection), so a
/// stored session records which backends it was configured to use. This lives in the wiring
/// layer (not core) because it reads `OTTO_*` / `ANTHROPIC_API_KEY`.
pub fn session_config() -> serde_json::Value {
    let ollama = std::env::var("OTTO_OLLAMA").as_deref() == Ok("1");
    let anthropic = std::env::var("ANTHROPIC_API_KEY")
        .map(|k| !k.is_empty())
        .unwrap_or(false);
    serde_json::json!({
        "ollama": ollama,
        "anthropic": anthropic,
        // Record the EFFECTIVE model (the build_router default when the env var is unset),
        // so a restored session's config reflects the routing it actually used.
        "ollama_model": std::env::var("OTTO_OLLAMA_MODEL")
            .unwrap_or_else(|_| DEFAULT_OLLAMA_MODEL.to_string()),
        "anthropic_model": std::env::var("OTTO_ANTHROPIC_MODEL")
            .unwrap_or_else(|_| DEFAULT_ANTHROPIC_MODEL.to_string()),
    })
}

/// Pure capability derivation from raw env inputs. Kept separate from `build_capabilities`
/// so the mapping is unit-testable without mutating process-global env (which would race the
/// env-reading tests in this binary). `engine_remote` is always false here: `otto serve` is
/// the local engine; the promote path (sub-project F) provisions a separate remote engine
/// that computes its own manifest with `engine_remote = true`.
fn capabilities_from_env(
    otto_ollama: Option<&str>,
    anthropic_key: Option<&str>,
    sandbox: bool,
) -> CapabilitiesManifest {
    CapabilitiesManifest {
        engine_remote: false,
        local_llm: otto_ollama == Some("1"),
        remote_llm: anthropic_key.map(|k| !k.is_empty()).unwrap_or(false),
        sandbox,
    }
}

/// Derive the running engine's capabilities from the environment `build_router` reads, plus
/// the OS sandbox probe. Lives in the wiring layer (not core) because it reads `OTTO_*` /
/// `ANTHROPIC_API_KEY`. Mirrors `session_config`'s predicates so a session's recorded config
/// and its reported capabilities stay consistent.
pub fn build_capabilities() -> CapabilitiesManifest {
    capabilities_from_env(
        std::env::var("OTTO_OLLAMA").ok().as_deref(),
        std::env::var("ANTHROPIC_API_KEY").ok().as_deref(),
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

    #[tokio::test]
    async fn default_build_router_is_offline_and_deterministic() {
        // Ensure the env that would select real backends is absent for this test.
        // SAFETY: this is the only test in this lib's test binary, so no other test
        // races on these process-global env vars. Do not add a test here that reads
        // OTTO_OLLAMA / ANTHROPIC_API_KEY without revisiting this.
        unsafe {
            std::env::remove_var("OTTO_OLLAMA");
            std::env::remove_var("ANTHROPIC_API_KEY");
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

    #[test]
    fn capabilities_from_env_maps_flags() {
        // Pure mapping — takes raw env inputs, touches no process-global env, so it does
        // NOT race the env-reading router test in this same binary.
        // Nothing set → fully offline, local engine, no sandbox.
        assert_eq!(
            capabilities_from_env(None, None, false),
            CapabilitiesManifest {
                engine_remote: false,
                local_llm: false,
                remote_llm: false,
                sandbox: false,
            }
        );
        // OTTO_OLLAMA must equal exactly "1" to count as a local LLM.
        assert!(capabilities_from_env(Some("1"), None, false).local_llm);
        assert!(!capabilities_from_env(Some("0"), None, false).local_llm);
        // A non-empty ANTHROPIC_API_KEY means a remote LLM; an empty one does not.
        assert!(capabilities_from_env(None, Some("sk-xyz"), false).remote_llm);
        assert!(!capabilities_from_env(None, Some(""), false).remote_llm);
        // sandbox passes through unchanged.
        assert!(capabilities_from_env(None, None, true).sandbox);
    }
}
