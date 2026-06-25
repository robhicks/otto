//! `task`: a built-in tool that dispatches a named custom agent as a depth-1 sub-turn. The
//! dispatched agent gets a tool view filtered to its `tools` allowlist (shared gate/ask, so
//! the sensitive-path floor is preserved). The base registry passed in never contains `task`,
//! so a dispatched agent cannot re-dispatch.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use otto_engine_core::registry::AgentRegistry;
use otto_engine_core::router::Router;
use otto_engine_core::tool::{Tool, ToolRegistry};
use otto_engine_core::traits::{AgentCtx, WorkspaceRead};
use otto_engine_core::types::{AgentOutput, AgentRequest};
use otto_protocol::Role;
use serde_json::{Value, json};

/// Dispatches `Role::Custom(<agent>)` agents. Holds shared engine deps because a `Tool` only
/// receives JSON args — it has no `AgentCtx` of its own.
pub struct TaskTool {
    router: Arc<dyn Router>,
    workspace: Arc<dyn WorkspaceRead>,
    agents: Arc<AgentRegistry>,
    base_tools: Arc<ToolRegistry>,
    allowlists: HashMap<String, Option<Vec<String>>>,
}

impl TaskTool {
    pub fn new(
        router: Arc<dyn Router>,
        workspace: Arc<dyn WorkspaceRead>,
        agents: Arc<AgentRegistry>,
        base_tools: Arc<ToolRegistry>,
        allowlists: HashMap<String, Option<Vec<String>>>,
    ) -> Self {
        Self {
            router,
            workspace,
            agents,
            base_tools,
            allowlists,
        }
    }

    /// The agent's allowlist entry: `Some(Some(list))` = filtered to `list`, `Some(None)` =
    /// all base tools, `None` = the agent has no entry at all (a wiring error → fail closed).
    fn allowlist_for(&self, name: &str) -> Option<Option<Vec<String>>> {
        self.allowlists.get(name).cloned()
    }

    /// Drops `task` from a tool-name list so a dispatched agent can never re-dispatch
    /// (depth-1 by construction, independent of what the base registry contains).
    fn without_task(names: Vec<String>) -> Vec<String> {
        names.into_iter().filter(|n| n != "task").collect()
    }
}

#[async_trait]
impl Tool for TaskTool {
    fn name(&self) -> &str {
        "task"
    }

    async fn call(&self, args: Value) -> anyhow::Result<Value> {
        let name = args
            .get("agent")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("task: missing string `agent`"))?;
        let prompt = args
            .get("prompt")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("task: missing string `prompt`"))?;

        let agent = self.agents.get(&Role::Custom(name.to_string()))?;

        let sub_tools = match self.allowlist_for(name) {
            Some(Some(allowed)) => self.base_tools.subset(&Self::without_task(allowed)),
            Some(None) => self
                .base_tools
                .subset(&Self::without_task(self.base_tools.tool_names())),
            None => {
                anyhow::bail!("task: no allowlist entry for agent '{name}' (internal wiring error)")
            }
        };

        let ctx = AgentCtx::new(self.router.as_ref(), self.workspace.as_ref(), &sub_tools);
        let out = agent
            .run(
                AgentRequest::Task {
                    prompt: prompt.to_string(),
                },
                &ctx,
            )
            .await?;
        match out {
            AgentOutput::Task { text } => Ok(json!({ "text": text })),
            other => anyhow::bail!("task: agent returned non-Task output: {other:?}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_def::CustomAgentDef;
    use crate::markdown_agent::MarkdownAgent;
    use otto_engine_core::router::RouteHints;
    use otto_engine_core::tool::{Decision, DenyAsk, PermissionGate};
    use otto_engine_core::types::{CompleteRequest, CompleteResponse};
    use std::path::{Path, PathBuf};

    struct EchoRouter;
    #[async_trait]
    impl Router for EchoRouter {
        async fn complete(
            &self,
            req: CompleteRequest,
            _hints: RouteHints,
        ) -> anyhow::Result<CompleteResponse> {
            Ok(CompleteResponse {
                text: req.prompt,
                usage: None,
            })
        }
    }

    struct StubWorkspace;
    #[async_trait]
    impl WorkspaceRead for StubWorkspace {
        async fn read(&self, _p: &Path) -> anyhow::Result<Vec<u8>> {
            Ok(Vec::new())
        }
        async fn list(&self, _g: &str) -> anyhow::Result<Vec<PathBuf>> {
            Ok(Vec::new())
        }
    }

    struct AllowAll;
    impl PermissionGate for AllowAll {
        fn evaluate(&self, _t: &str, _a: &Value) -> Decision {
            Decision::Allow
        }
    }

    fn tool() -> TaskTool {
        let mut reg = AgentRegistry::new();
        reg.register(
            Role::Custom("echoer".into()),
            Arc::new(MarkdownAgent::new(CustomAgentDef {
                name: "echoer".into(),
                description: "d".into(),
                tools: Some(vec!["fs.read".into()]),
                model: None,
                system_prompt: "SYS".into(),
            })),
        );
        let base = ToolRegistry::new(Arc::new(AllowAll), Arc::new(DenyAsk));
        let mut allowlists = HashMap::new();
        allowlists.insert("echoer".to_string(), Some(vec!["fs.read".to_string()]));
        TaskTool::new(
            Arc::new(EchoRouter),
            Arc::new(StubWorkspace),
            Arc::new(reg),
            Arc::new(base),
            allowlists,
        )
    }

    #[tokio::test]
    async fn dispatches_named_agent_and_returns_text() {
        let out = tool()
            .call(json!({ "agent": "echoer", "prompt": "hello" }))
            .await
            .unwrap();
        let text = out["text"].as_str().unwrap();
        assert!(text.contains("SYS"));
        assert!(text.contains("hello"));
    }

    #[tokio::test]
    async fn unknown_agent_errors() {
        let err = tool()
            .call(json!({ "agent": "ghost", "prompt": "x" }))
            .await;
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn missing_args_error() {
        assert!(tool().call(json!({ "prompt": "x" })).await.is_err());
        assert!(tool().call(json!({ "agent": "echoer" })).await.is_err());
    }

    struct ProbeAgent;
    #[async_trait]
    impl otto_engine_core::traits::Agent for ProbeAgent {
        async fn run(&self, req: AgentRequest, ctx: &AgentCtx) -> anyhow::Result<AgentOutput> {
            let AgentRequest::Task { prompt } = req else {
                anyhow::bail!("probe expects Task");
            };
            // `prompt` names a tool to attempt; report whether it was reachable.
            let reachable = ctx.tools().call(&prompt, json!({})).await.is_ok();
            Ok(AgentOutput::Task {
                text: format!("reachable={reachable}"),
            })
        }
    }

    struct PingTool;
    #[async_trait]
    impl Tool for PingTool {
        fn name(&self) -> &str {
            "ping"
        }
        async fn call(&self, _a: Value) -> anyhow::Result<Value> {
            Ok(json!("pong"))
        }
    }

    #[tokio::test]
    async fn dispatched_agent_only_sees_allowlisted_tools() {
        let mut reg = AgentRegistry::new();
        reg.register(Role::Custom("probe".into()), Arc::new(ProbeAgent));

        let mut base = ToolRegistry::new(Arc::new(AllowAll), Arc::new(DenyAsk));
        base.register(Arc::new(PingTool));

        // Allowlist is empty → the agent should NOT reach `ping`.
        let mut allowlists = HashMap::new();
        allowlists.insert("probe".to_string(), Some(Vec::<String>::new()));
        let denied = TaskTool::new(
            Arc::new(EchoRouter),
            Arc::new(StubWorkspace),
            Arc::new(reg),
            Arc::new(base),
            allowlists,
        );
        let out = denied
            .call(json!({ "agent": "probe", "prompt": "ping" }))
            .await
            .unwrap();
        assert_eq!(out["text"], "reachable=false");
    }

    #[tokio::test]
    async fn none_allowlist_grants_all_base_tools() {
        let mut reg = AgentRegistry::new();
        reg.register(Role::Custom("probe".into()), Arc::new(ProbeAgent));

        let mut base = ToolRegistry::new(Arc::new(AllowAll), Arc::new(DenyAsk));
        base.register(Arc::new(PingTool));

        // `None` entry → the agent receives all base tools.
        let mut allowlists = HashMap::new();
        allowlists.insert("probe".to_string(), None);
        let granted = TaskTool::new(
            Arc::new(EchoRouter),
            Arc::new(StubWorkspace),
            Arc::new(reg),
            Arc::new(base),
            allowlists,
        );
        let out = granted
            .call(json!({ "agent": "probe", "prompt": "ping" }))
            .await
            .unwrap();
        assert_eq!(out["text"], "reachable=true");
    }
}
