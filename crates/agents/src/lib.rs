//! otto's atomic agents. `Planner` and `Coder` are real LLM-backed agents — each prompts the
//! router for structured JSON and parses it, falling back safely when no JSON is returned.
//! `Verifier` is real too: it detects the project ecosystem and runs that ecosystem's test
//! command (e.g. `cargo test`, `go test`, `npm test`) via the sandboxed `bash` tool. All four
//! spine agents (`Planner`, `ContextFinder`, `Coder`, `Verifier`) are real.

pub mod coder;
pub mod context_finder;
pub mod parse;
pub mod planner;
pub mod verifier;

pub use coder::Coder;
pub use context_finder::ContextFinder;
pub use planner::Planner;
pub use verifier::Verifier;

#[cfg(test)]
mod readonly_view_tests {
    use otto_engine_core::tool::{DenyAsk, ToolRegistry};
    use otto_engine_core::traits::{AgentCtx, Workspace, WorkspaceRead};
    use otto_engine_core::types::Edit;
    use otto_providers::LocalProvider;
    use otto_router::SingleProviderRouter;
    use otto_tools::DefaultPermissionGate;
    use otto_workspace::LocalWorkspace;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    #[tokio::test]
    async fn agentctx_exposes_a_working_readonly_workspace_view() {
        let dir = tempfile::tempdir().unwrap();
        let ws = LocalWorkspace::new(dir.path());
        ws.apply_edit(&Edit {
            path: PathBuf::from("seed.txt"),
            new_contents: "hi".to_string(),
        })
        .await
        .unwrap();

        let router = SingleProviderRouter::new(Arc::new(LocalProvider::new()));
        let tools = ToolRegistry::new(Arc::new(DefaultPermissionGate::new()), Arc::new(DenyAsk));
        let ctx = AgentCtx::new(&router, &ws, &tools);

        // The agent-facing view is read-only and can read.
        let view: &dyn WorkspaceRead = ctx.workspace();
        let bytes = view.read(Path::new("seed.txt")).await.unwrap();
        assert_eq!(bytes, b"hi");
        // `view` has no `apply_edit` — the ungated write path is gone at the type level.
    }
}
