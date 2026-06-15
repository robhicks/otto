//! The Coder agent: prompts the router for the file edits that accomplish the goal, parsing a
//! structured JSON response. Falls back to NO edits on parse failure (the turn proceeds, the
//! orchestrator writes nothing). Emitted edits are gated by the orchestrator before applying.

use std::path::PathBuf;

use async_trait::async_trait;
use otto_engine_core::router::{RouteHints, TaskKind};
use otto_engine_core::traits::{Agent, AgentCtx};
use otto_engine_core::types::{AgentOutput, AgentRequest, CompleteRequest, Edit};
use serde::Deserialize;

use crate::parse::extract_json;

pub struct Coder;

#[derive(Deserialize)]
struct CodeResponse {
    edits: Vec<EditDto>,
}

#[derive(Deserialize)]
struct EditDto {
    path: String,
    contents: String,
}

fn code_prompt(goal: &str, context: &[PathBuf], feedback: Option<&str>) -> String {
    let files = if context.is_empty() {
        "(none)".to_string()
    } else {
        context
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut prompt = format!(
        "You are otto's coder. Produce the complete file edits that accomplish the goal.\n\
         Goal: {goal}\n\
         Existing files: {files}\n\
         Respond ONLY with valid JSON matching this schema:\n\
         edits: array of objects, each with a string field named path (a relative path) and \
         a string field named contents (the full new file contents)."
    );
    if let Some(detail) = feedback {
        prompt.push_str(&format!(
            "\nThe previous attempt failed verification with this output; fix it:\n{detail}"
        ));
    }
    prompt
}

#[async_trait]
impl Agent for Coder {
    async fn run(&self, req: AgentRequest, ctx: &AgentCtx) -> anyhow::Result<AgentOutput> {
        let AgentRequest::Code {
            goal,
            context,
            feedback,
            prior_failures,
        } = req
        else {
            anyhow::bail!("Coder received a non-Code request");
        };
        let completion = ctx
            .router()
            .complete(
                CompleteRequest {
                    prompt: code_prompt(&goal, &context, feedback.as_deref()),
                },
                RouteHints {
                    task_kind: TaskKind::Edit,
                    prior_failures,
                    ..RouteHints::default()
                },
            )
            .await?;
        // Parse the edits; on any failure produce no edits.
        let edits = match extract_json::<CodeResponse>(&completion.text) {
            Ok(code) => code
                .edits
                .into_iter()
                .map(|e| Edit {
                    path: PathBuf::from(e.path),
                    new_contents: e.contents,
                })
                .collect(),
            Err(_) => Vec::new(),
        };
        Ok(AgentOutput::Code { edits })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use otto_engine_core::tool::{DenyAsk, ToolRegistry};
    use otto_providers::{LocalProvider, ScriptedProvider};
    use otto_router::SingleProviderRouter;
    use otto_tools::DefaultPermissionGate;
    use otto_workspace::LocalWorkspace;
    use std::sync::Arc;

    async fn run_coder_with(router: &SingleProviderRouter, feedback: Option<String>) -> Vec<Edit> {
        let dir = tempfile::tempdir().unwrap();
        let ws = LocalWorkspace::new(dir.path());
        let tools = ToolRegistry::new(Arc::new(DefaultPermissionGate::new()), Arc::new(DenyAsk));
        let ctx = AgentCtx::new(router, &ws, &tools);
        let out = Coder
            .run(
                AgentRequest::Code {
                    goal: "add a greeting".to_string(),
                    context: Vec::new(),
                    feedback,
                    prior_failures: 0,
                },
                &ctx,
            )
            .await
            .unwrap();
        match out {
            AgentOutput::Code { edits } => edits,
            other => panic!("expected Code, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn parses_edits_from_json() {
        let provider = ScriptedProvider::new("{}").on(
            "edits",
            r#"{"edits": [{"path": "greeting.txt", "contents": "hello world"}]}"#,
        );
        let router = SingleProviderRouter::new(Arc::new(provider));
        let edits = run_coder_with(&router, None).await;
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].path, PathBuf::from("greeting.txt"));
        assert_eq!(edits[0].new_contents, "hello world");
    }

    #[tokio::test]
    async fn falls_back_to_no_edits_when_unparseable() {
        let router = SingleProviderRouter::new(Arc::new(LocalProvider::new()));
        let edits = run_coder_with(&router, None).await;
        assert!(edits.is_empty());
    }

    #[tokio::test]
    async fn feedback_is_included_in_the_prompt() {
        // The scripted rule fires only if the prompt contains the feedback marker; otherwise
        // the default ("{}") yields no edits. A non-empty result proves feedback was threaded in.
        let provider = ScriptedProvider::new("{}").on(
            "MARKER-9F3",
            r#"{"edits": [{"path": "fixed.txt", "contents": "ok"}]}"#,
        );
        let router = SingleProviderRouter::new(Arc::new(provider));
        let edits = run_coder_with(&router, Some("error: MARKER-9F3 something broke".into())).await;
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].path, PathBuf::from("fixed.txt"));
    }
}
