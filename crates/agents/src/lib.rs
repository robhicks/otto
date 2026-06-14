//! Walking-skeleton atomic agents. These return canned/structured output so the
//! orchestrator spine can be proven before real LLM-backed agents arrive.

pub mod parse;

use std::path::PathBuf;

use async_trait::async_trait;
use otto_engine_core::router::{RouteHints, TaskKind};
use otto_engine_core::traits::{Agent, AgentCtx};
use otto_engine_core::types::{AgentOutput, AgentRequest, CompleteRequest, Edit, Milestone};
use serde_json::Value;

/// Turns a goal into a single milestone.
pub struct StubPlanner;

#[async_trait]
impl Agent for StubPlanner {
    async fn run(&self, req: AgentRequest, _ctx: &AgentCtx) -> anyhow::Result<AgentOutput> {
        let AgentRequest::Plan { goal } = req else {
            anyhow::bail!("StubPlanner received a non-Plan request");
        };
        Ok(AgentOutput::Plan {
            milestones: vec![Milestone { description: goal }],
        })
    }
}

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

/// Calls the provider with the goal and writes the completion to `otto_output.txt`.
/// This is the agent that exercises the Provider seam end-to-end.
pub struct EchoCoder;

#[async_trait]
impl Agent for EchoCoder {
    async fn run(&self, req: AgentRequest, ctx: &AgentCtx) -> anyhow::Result<AgentOutput> {
        let AgentRequest::Code { goal, .. } = req else {
            anyhow::bail!("EchoCoder received a non-Code request");
        };
        let completion = ctx
            .router()
            .complete(
                CompleteRequest { prompt: goal },
                RouteHints {
                    task_kind: TaskKind::Edit,
                    ..RouteHints::default()
                },
            )
            .await?;
        Ok(AgentOutput::Code {
            edits: vec![Edit {
                path: PathBuf::from("otto_output.txt"),
                new_contents: completion.text,
            }],
        })
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
    async fn planner_produces_one_milestone_from_goal() {
        let router = SingleProviderRouter::new(Arc::new(LocalProvider::new()));
        let dir = tempfile::tempdir().unwrap();
        let ws = LocalWorkspace::new(dir.path());
        let tools = ToolRegistry::new(Arc::new(DefaultPermissionGate::new()), Arc::new(DenyAsk));
        let out = StubPlanner
            .run(
                AgentRequest::Plan {
                    goal: "add a greeting".to_string(),
                },
                &ctx(&router, &ws, &tools),
            )
            .await
            .unwrap();
        match out {
            AgentOutput::Plan { milestones } => {
                assert_eq!(milestones.len(), 1);
                assert_eq!(milestones[0].description, "add a greeting");
            }
            other => panic!("expected Plan output, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn coder_turns_completion_into_an_edit() {
        let router = SingleProviderRouter::new(Arc::new(LocalProvider::new()));
        let dir = tempfile::tempdir().unwrap();
        let ws = LocalWorkspace::new(dir.path());
        let tools = ToolRegistry::new(Arc::new(DefaultPermissionGate::new()), Arc::new(DenyAsk));
        let out = EchoCoder
            .run(
                AgentRequest::Code {
                    goal: "add a greeting".to_string(),
                    context: Vec::new(),
                },
                &ctx(&router, &ws, &tools),
            )
            .await
            .unwrap();
        match out {
            AgentOutput::Code { edits } => {
                assert_eq!(edits.len(), 1);
                assert_eq!(edits[0].path, PathBuf::from("otto_output.txt"));
                assert!(edits[0].new_contents.contains("add a greeting"));
            }
            other => panic!("expected Code output, got {other:?}"),
        }
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
