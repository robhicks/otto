//! The Coder agent: prompts the router for the file edits that accomplish the goal, parsing a
//! structured JSON response. Falls back to NO edits on parse failure (the turn proceeds, the
//! orchestrator writes nothing). Emitted edits are gated by the orchestrator before applying.

use std::path::PathBuf;

use async_trait::async_trait;
use otto_engine_core::router::{RouteHints, TaskKind};
use otto_engine_core::traits::{Agent, AgentCtx};
use otto_engine_core::types::{AgentOutput, AgentRequest, CompleteRequest, Edit, Milestone};
use serde::Deserialize;
use serde_json::{Value, json};

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

/// Context injection budgets (chars; ~bytes for ASCII source).
const MAX_CONTEXT_FILES: usize = 8;
const MAX_FILE_CHARS: usize = 8_000;
const MAX_TOTAL_CHARS: usize = 32_000;

/// Truncate to at most `max` chars on a char boundary, appending a marker when cut.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push_str("\n… (truncated)");
        out
    }
}

/// Read up to the budget of context files via `fs.read`, returning (path, contents). Files that
/// are unreadable, gate-denied, or non-UTF8 are skipped; the total-char budget stops the loop.
async fn read_context(ctx: &AgentCtx<'_>, context: &[PathBuf]) -> Vec<(PathBuf, String)> {
    let mut out = Vec::new();
    let mut total = 0usize;
    for path in context.iter().take(MAX_CONTEXT_FILES) {
        let content = match ctx
            .tools()
            .call("fs.read", json!({ "path": path.display().to_string() }))
            .await
        {
            Ok(Value::Object(map)) => map
                .get("content")
                .and_then(Value::as_str)
                .map(str::to_string),
            _ => None,
        };
        let Some(content) = content else { continue };
        let content = truncate_chars(&content, MAX_FILE_CHARS);
        let len = content.chars().count();
        if total + len > MAX_TOTAL_CHARS {
            break;
        }
        total += len;
        out.push((path.clone(), content));
    }
    out
}

fn code_prompt(
    goal: &str,
    milestones: &[Milestone],
    files: &[(PathBuf, String)],
    feedback: Option<&str>,
) -> String {
    let plan_block = if milestones.is_empty() {
        "(none)".to_string()
    } else {
        milestones
            .iter()
            .enumerate()
            .map(|(i, m)| format!("{}. {}", i + 1, m.description))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let context_block = if files.is_empty() {
        "(none)".to_string()
    } else {
        files
            .iter()
            .map(|(p, c)| format!("--- {} ---\n{}", p.display(), c))
            .collect::<Vec<_>>()
            .join("\n\n")
    };
    let mut prompt = format!(
        "You are otto's coder. Produce the complete file edits that accomplish the goal.\n\
         Goal: {goal}\n\
         Plan (milestones to accomplish):\n{plan_block}\n\
         Relevant files (each shown as a path then its current contents):\n{context_block}\n\
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
            milestones,
            context,
            feedback,
            prior_failures,
        } = req
        else {
            anyhow::bail!("Coder received a non-Code request");
        };
        let files = read_context(ctx, &context).await;
        let completion = ctx
            .router()
            .complete(
                CompleteRequest {
                    prompt: code_prompt(&goal, &milestones, &files, feedback.as_deref()),
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
                    milestones: Vec::new(),
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
    async fn reads_context_file_contents_into_prompt() {
        use otto_engine_core::tool::Tool;
        use otto_engine_core::traits::Workspace;
        use otto_engine_core::types::Edit;
        use otto_tools::FsReadTool;

        let dir = tempfile::tempdir().unwrap();
        let ws = LocalWorkspace::new(dir.path());
        ws.apply_edit(&Edit {
            path: PathBuf::from("src/lib.rs"),
            new_contents: "fn special_marker_42() {}".to_string(),
        })
        .await
        .unwrap();

        let mut tools =
            ToolRegistry::new(Arc::new(DefaultPermissionGate::new()), Arc::new(DenyAsk));
        let ws_arc: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(dir.path()));
        tools.register(Arc::new(FsReadTool::new(ws_arc)) as Arc<dyn Tool>);

        // The scripted rule fires ONLY if the prompt contains the file's contents, proving the
        // Coder read and injected them.
        let provider = ScriptedProvider::new("{}").on(
            "special_marker_42",
            r#"{"edits": [{"path": "out.txt", "contents": "ok"}]}"#,
        );
        let router = SingleProviderRouter::new(Arc::new(provider));
        let ctx = AgentCtx::new(&router, &ws, &tools);
        let out = Coder
            .run(
                AgentRequest::Code {
                    goal: "update".to_string(),
                    milestones: Vec::new(),
                    context: vec![PathBuf::from("src/lib.rs")],
                    feedback: None,
                    prior_failures: 0,
                },
                &ctx,
            )
            .await
            .unwrap();
        match out {
            AgentOutput::Code { edits } => assert_eq!(edits.len(), 1),
            other => panic!("expected Code, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn includes_milestones_in_prompt() {
        use otto_engine_core::types::Milestone;
        let dir = tempfile::tempdir().unwrap();
        let ws = LocalWorkspace::new(dir.path());
        let tools = ToolRegistry::new(Arc::new(DefaultPermissionGate::new()), Arc::new(DenyAsk));
        // The scripted rule fires ONLY if the milestone description reached the prompt.
        let provider = ScriptedProvider::new("{}").on(
            "MILESTONE_MARKER_7",
            r#"{"edits": [{"path": "out.txt", "contents": "ok"}]}"#,
        );
        let router = SingleProviderRouter::new(Arc::new(provider));
        let ctx = AgentCtx::new(&router, &ws, &tools);
        let out = Coder
            .run(
                AgentRequest::Code {
                    goal: "build it".to_string(),
                    milestones: vec![Milestone {
                        description: "implement MILESTONE_MARKER_7".to_string(),
                    }],
                    context: Vec::new(),
                    feedback: None,
                    prior_failures: 0,
                },
                &ctx,
            )
            .await
            .unwrap();
        match out {
            AgentOutput::Code { edits } => assert_eq!(edits.len(), 1),
            other => panic!("expected Code, got {other:?}"),
        }
    }

    #[test]
    fn truncate_chars_caps_and_marks() {
        let s: String = "x".repeat(100);
        let t = truncate_chars(&s, 10);
        assert!(t.starts_with(&"x".repeat(10)));
        assert!(t.contains("truncated"));
        assert_eq!(truncate_chars("short", 10), "short");
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
