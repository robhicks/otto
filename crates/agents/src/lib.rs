//! Walking-skeleton atomic agents. These return canned/structured output so the
//! orchestrator spine can be proven before real LLM-backed agents arrive.

use std::path::PathBuf;

use async_trait::async_trait;
use otto_engine_core::traits::{Agent, AgentCtx};
use otto_engine_core::types::{AgentOutput, AgentRequest, CompleteRequest, Edit, Milestone};

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

/// Returns an empty context set in the skeleton.
pub struct StubContextFinder;

#[async_trait]
impl Agent for StubContextFinder {
    async fn run(&self, req: AgentRequest, _ctx: &AgentCtx) -> anyhow::Result<AgentOutput> {
        let AgentRequest::FindContext { .. } = req else {
            anyhow::bail!("StubContextFinder received a non-FindContext request");
        };
        Ok(AgentOutput::Context { files: Vec::new() })
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
            .provider
            .complete(CompleteRequest { prompt: goal })
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
    use otto_providers::LocalProvider;
    use otto_workspace::LocalWorkspace;

    fn ctx<'a>(provider: &'a LocalProvider, workspace: &'a LocalWorkspace) -> AgentCtx<'a> {
        AgentCtx {
            provider,
            workspace,
        }
    }

    #[tokio::test]
    async fn planner_produces_one_milestone_from_goal() {
        let provider = LocalProvider::new();
        let dir = tempfile::tempdir().unwrap();
        let ws = LocalWorkspace::new(dir.path());
        let out = StubPlanner
            .run(
                AgentRequest::Plan {
                    goal: "add a greeting".to_string(),
                },
                &ctx(&provider, &ws),
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
        let provider = LocalProvider::new();
        let dir = tempfile::tempdir().unwrap();
        let ws = LocalWorkspace::new(dir.path());
        let out = EchoCoder
            .run(
                AgentRequest::Code {
                    goal: "add a greeting".to_string(),
                    context: Vec::new(),
                },
                &ctx(&provider, &ws),
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
}
