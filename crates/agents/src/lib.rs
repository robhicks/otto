//! Walking-skeleton atomic agents. These return canned/structured output so the
//! orchestrator spine can be proven before real LLM-backed agents arrive.

pub mod coder;
pub mod parse;
pub mod planner;

pub use coder::Coder;
pub use planner::Planner;

use async_trait::async_trait;
use otto_engine_core::traits::{Agent, AgentCtx};
use otto_engine_core::types::{AgentOutput, AgentRequest};
use serde_json::Value;

/// Lists the workspace's top-level files via the `fs.list` tool and returns them as context.
/// Falls back to an empty set if the tool is unavailable or errors (skeleton-friendly).
pub struct StubContextFinder;

#[async_trait]
impl Agent for StubContextFinder {
    async fn run(&self, req: AgentRequest, ctx: &AgentCtx) -> anyhow::Result<AgentOutput> {
        let AgentRequest::FindContext { .. } = req else {
            anyhow::bail!("StubContextFinder received a non-FindContext request");
        };
        let files = match ctx.tools().call("fs.list", serde_json::json!({})).await {
            Ok(Value::Object(map)) => map
                .get("paths")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(std::path::PathBuf::from))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        Ok(AgentOutput::Context { files })
    }
}

/// Always reports success in the skeleton.
pub struct StubVerifier;

#[async_trait]
impl Agent for StubVerifier {
    async fn run(&self, req: AgentRequest, _ctx: &AgentCtx) -> anyhow::Result<AgentOutput> {
        let AgentRequest::Verify = req else {
            anyhow::bail!("StubVerifier received a non-Verify request");
        };
        Ok(AgentOutput::Verify {
            ok: true,
            detail: "skeleton verifier: no checks run".to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use otto_engine_core::tool::{DenyAsk, ToolRegistry};
    use otto_providers::LocalProvider;
    use otto_router::SingleProviderRouter;
    use otto_tools::{DefaultPermissionGate, FsListTool};
    use otto_workspace::LocalWorkspace;
    use std::sync::Arc;

    fn ctx<'a>(
        router: &'a SingleProviderRouter,
        workspace: &'a LocalWorkspace,
        tools: &'a ToolRegistry,
    ) -> AgentCtx<'a> {
        AgentCtx::new(router, workspace, tools)
    }

    #[tokio::test]
    async fn context_finder_lists_workspace_files_through_tools() {
        let router = SingleProviderRouter::new(Arc::new(LocalProvider::new()));
        let dir = tempfile::tempdir().unwrap();
        let ws_concrete = LocalWorkspace::new(dir.path());
        // Seed a file via a write through the workspace trait used by the tool.
        use otto_engine_core::traits::Workspace;
        use otto_engine_core::types::Edit;
        ws_concrete
            .apply_edit(&Edit {
                path: std::path::PathBuf::from("seed.txt"),
                new_contents: "x".into(),
            })
            .await
            .unwrap();

        // Build a registry with fs.list over the SAME workspace path.
        let ws_for_tool: Arc<dyn otto_engine_core::traits::Workspace> =
            Arc::new(LocalWorkspace::new(dir.path()));
        let mut registry =
            ToolRegistry::new(Arc::new(DefaultPermissionGate::new()), Arc::new(DenyAsk));
        registry.register(Arc::new(FsListTool::new(ws_for_tool)));

        let out = StubContextFinder
            .run(
                AgentRequest::FindContext { goal: "g".into() },
                &ctx(&router, &ws_concrete, &registry),
            )
            .await
            .unwrap();
        match out {
            AgentOutput::Context { files } => {
                assert!(files.contains(&std::path::PathBuf::from("seed.txt")));
            }
            other => panic!("expected Context, got {other:?}"),
        }
    }
}
