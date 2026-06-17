//! Engine wiring: assemble the default agent registry and run a turn end-to-end.

use std::path::PathBuf;
use std::sync::Arc;

use otto_agents::{Coder, ContextFinder, Planner, Verifier};
use otto_engine_core::tool::{AllowListAskResolver, AskResolver, DenyAsk, ToolRegistry};
use otto_engine_core::traits::{Provider, Workspace, WorkspaceRead};
use otto_engine_core::{AgentRegistry, Router, TurnOutcome};
use otto_persistence::SessionStore;
use otto_protocol::{Event, Role};
use otto_providers::{AnthropicProvider, LocalProvider, OllamaProvider};
use otto_router::{BrainBlendRouter, SingleProviderRouter};
use otto_tools::{
    BashTool, DefaultPermissionGate, FsListTool, FsReadTool, FsWriteTool, SandboxPolicy,
    os_sandbox_available,
};

mod mcp;
mod remote;
mod serve;
mod service;

pub use mcp::{McpConnection, connect_fs as mcp_connect_fs};
pub use remote::{
    LoopbackTarget, PromoteBundle, RemoteHandle, RemoteTarget, UnsupportedTarget, promote,
};
pub use serve::{app as serve_app, resolve_tls_paths, run as serve_run};
pub use service::{CollectingSink, EngineService, EventSink};

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

/// Build the default tool registry. Always includes the sensitive-path-floor gate and the
/// in-process fs tools. A sandboxed `bash` tool is registered ONLY when an OS sandbox backend
/// (bwrap/sandbox-exec) is available; in that case the `Ask` verdict the gate gives `bash` is
/// resolved by an allow-list resolver (safe because the registered bash is OS-confined — and
/// the no-orphans-on-timeout guarantee holds because we only ever wire the `Os` policy, never
/// `None`). With no sandbox backend, `bash` is absent and the resolver denies all `Ask`
/// (fail-closed).
pub fn build_tool_registry(workspace: Arc<dyn Workspace>, root: PathBuf) -> ToolRegistry {
    let sandboxed = os_sandbox_available();
    let ask: Arc<dyn AskResolver> = if sandboxed {
        Arc::new(AllowListAskResolver::new(vec!["bash".to_string()]))
    } else {
        Arc::new(DenyAsk)
    };

    let mut registry = ToolRegistry::new(Arc::new(DefaultPermissionGate::new()), ask);
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

/// Run one turn for `goal` through an `EngineService` backed by `store`, returning the
/// sequenced events and the outcome. A thin wrapper: build the service, create a session,
/// run one prompt with a collecting sink.
pub async fn run_goal(
    goal: &str,
    store: Arc<dyn SessionStore>,
    router: Arc<dyn Router>,
    workspace: Arc<dyn Workspace>,
    tools: Arc<ToolRegistry>,
) -> anyhow::Result<(Vec<Event>, TurnOutcome)> {
    let service = EngineService::new(
        store,
        Arc::new(build_default_registry()),
        router,
        workspace,
        tools,
    );
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
}
