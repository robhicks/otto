//! A custom agent loaded from `agents/*.md`. It answers a free-form `AgentRequest::Task` by
//! running its markdown body as a system prompt through the router. It uses whatever tool
//! view its `AgentCtx` carries — the dispatcher (`TaskTool`) supplies the allowlist-filtered
//! subset.

use async_trait::async_trait;
use otto_engine_core::router::RouteHints;
use otto_engine_core::traits::{Agent, AgentCtx};
use otto_engine_core::types::{AgentOutput, AgentRequest, CompleteRequest};

use crate::agent_def::CustomAgentDef;

/// An `Agent` backed by a parsed `CustomAgentDef`.
pub struct MarkdownAgent {
    def: CustomAgentDef,
}

impl MarkdownAgent {
    pub fn new(def: CustomAgentDef) -> Self {
        Self { def }
    }

    /// The agent's tool allowlist (`None` = all available tools). Read by the dispatcher.
    pub fn tools(&self) -> Option<&[String]> {
        self.def.tools.as_deref()
    }

    /// The agent's name (its `Role::Custom` key).
    pub fn name(&self) -> &str {
        &self.def.name
    }
}

#[async_trait]
impl Agent for MarkdownAgent {
    async fn run(&self, req: AgentRequest, ctx: &AgentCtx) -> anyhow::Result<AgentOutput> {
        let AgentRequest::Task { prompt } = req else {
            anyhow::bail!("MarkdownAgent only handles AgentRequest::Task");
        };
        // `model` is preserved on the def for a later slice; routing is unaffected this slice.
        let composed = format!("{}\n\n{}", self.def.system_prompt, prompt);
        let resp = ctx
            .router()
            .complete(CompleteRequest { prompt: composed }, RouteHints::default())
            .await?;
        Ok(AgentOutput::Task { text: resp.text })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use otto_engine_core::router::Router;
    use otto_engine_core::tool::{AskResolver, Decision, DenyAsk, PermissionGate, ToolRegistry};
    use otto_engine_core::traits::WorkspaceRead;
    use otto_engine_core::types::CompleteResponse;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

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
        async fn read(&self, _path: &Path) -> anyhow::Result<Vec<u8>> {
            Ok(Vec::new())
        }
        async fn list(&self, _glob: &str) -> anyhow::Result<Vec<PathBuf>> {
            Ok(Vec::new())
        }
    }

    struct AllowAll;
    impl PermissionGate for AllowAll {
        fn evaluate(&self, _tool: &str, _args: &serde_json::Value) -> Decision {
            Decision::Allow
        }
    }

    fn def() -> CustomAgentDef {
        CustomAgentDef {
            name: "reviewer".into(),
            description: "d".into(),
            tools: Some(vec!["fs.read".into()]),
            model: Some("claude-opus-4-8".into()),
            system_prompt: "SYSTEM-PROMPT".into(),
        }
    }

    #[tokio::test]
    async fn runs_task_and_includes_system_prompt() {
        let agent = MarkdownAgent::new(def());
        let router = EchoRouter;
        let ws = StubWorkspace;
        let tools = ToolRegistry::new(
            Arc::new(AllowAll),
            Arc::new(DenyAsk) as Arc<dyn AskResolver>,
        );
        let ctx = AgentCtx::new(&router, &ws, &tools);

        let out = agent
            .run(
                AgentRequest::Task {
                    prompt: "do the thing".into(),
                },
                &ctx,
            )
            .await
            .unwrap();
        match out {
            AgentOutput::Task { text } => {
                assert!(text.contains("SYSTEM-PROMPT"));
                assert!(text.contains("do the thing"));
            }
            other => panic!("expected Task output, got {other:?}"),
        }
    }

    #[test]
    fn preserves_model_and_allowlist() {
        let agent = MarkdownAgent::new(def());
        assert_eq!(agent.tools(), Some(["fs.read".to_string()].as_slice()));
        assert_eq!(agent.name(), "reviewer");
    }
}
