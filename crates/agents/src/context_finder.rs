//! The ContextFinder agent: selects the workspace files relevant to a goal. A deterministic
//! lexical prefilter scores files by goal-keyword matches (path matches weighted higher than
//! content matches) and keeps the top candidates; the model is then asked to pick the most
//! relevant subset. If the model does not answer in schema (the default `LocalProvider` path,
//! or any parse failure) it falls back to the lexical top-N, so the offline path is fully
//! deterministic. Returns ranked relative paths; the Coder reads their contents.

use std::collections::HashSet;
use std::path::PathBuf;

use async_trait::async_trait;
use otto_engine_core::router::{RouteHints, TaskKind};
use otto_engine_core::traits::{Agent, AgentCtx};
use otto_engine_core::types::{AgentOutput, AgentRequest, CompleteRequest};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::parse::extract_json;

/// Candidate files kept after lexical scoring, before LLM selection.
const CANDIDATE_LIMIT: usize = 20;
/// Maximum files returned as context.
const SELECT_LIMIT: usize = 8;
/// Per-file content scanned for lexical scoring (chars).
const CONTENT_SCAN_CHARS: usize = 65_536;

pub struct ContextFinder;

#[derive(Deserialize)]
struct SelectResponse {
    files: Vec<String>,
}

/// Goal keywords: alphanumeric tokens, lowercased, length >= 3, minus a small stopword set,
/// de-duplicated (so a repeated word does not double-weight).
fn keywords(goal: &str) -> Vec<String> {
    const STOPWORDS: &[&str] = &[
        "the", "and", "for", "with", "that", "this", "add", "fix", "make", "use", "into", "from",
        "you",
    ];
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for tok in goal.split(|c: char| !c.is_alphanumeric()) {
        let t = tok.to_lowercase();
        if t.len() >= 3 && !STOPWORDS.contains(&t.as_str()) && seen.insert(t.clone()) {
            out.push(t);
        }
    }
    out
}

/// Lexical relevance score: 5x per path/filename hit + 1x per content hit, summed over keywords.
fn score_file(path: &str, content: Option<&str>, kws: &[String]) -> u64 {
    let path_l = path.to_lowercase();
    let content_l = content.map(|c| c.chars().take(CONTENT_SCAN_CHARS).collect::<String>());
    let mut total = 0u64;
    for kw in kws {
        let path_hits = path_l.matches(kw.as_str()).count() as u64;
        let content_hits = content_l
            .as_deref()
            .map(|c| c.to_lowercase().matches(kw.as_str()).count() as u64)
            .unwrap_or(0);
        total += 5 * path_hits + content_hits;
    }
    total
}

fn select_prompt(goal: &str, candidates: &[(String, u64)]) -> String {
    let listed = candidates
        .iter()
        .map(|(p, s)| format!("- {p} (score {s})"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "You are otto's context finder. From the candidate files below, choose up to {SELECT_LIMIT} \
         files most relevant to the goal, most relevant first.\n\
         Goal: {goal}\n\
         Candidates:\n{listed}\n\
         Respond ONLY with valid JSON: an object with a string-array field named files, each an \
         exact path copied from the candidates."
    )
}

#[async_trait]
impl Agent for ContextFinder {
    async fn run(&self, req: AgentRequest, ctx: &AgentCtx) -> anyhow::Result<AgentOutput> {
        let AgentRequest::FindContext { goal } = req else {
            anyhow::bail!("ContextFinder received a non-FindContext request");
        };

        // Enumerate the workspace recursively.
        let files: Vec<String> = match ctx.tools().call("fs.list", json!({ "glob": "**" })).await {
            Ok(Value::Object(map)) => map
                .get("paths")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
            _ => Vec::new(),
        };

        let kws = keywords(&goal);

        // Lexical scoring. Read each file via fs.read; a non-UTF8/unreadable file scores on its
        // path only.
        let mut scored: Vec<(String, u64)> = Vec::new();
        for path in &files {
            let content = match ctx.tools().call("fs.read", json!({ "path": path })).await {
                Ok(Value::Object(map)) => map
                    .get("content")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                _ => None,
            };
            let score = score_file(path, content.as_deref(), &kws);
            if score > 0 {
                scored.push((path.clone(), score));
            }
        }
        // Rank by score desc, tie-broken by path asc (deterministic), keep the top candidates.
        scored.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        scored.truncate(CANDIDATE_LIMIT);

        if scored.is_empty() {
            return Ok(AgentOutput::Context { files: Vec::new() });
        }

        // LLM rank/select over the candidates; fall back to the lexical top-N on any failure.
        let lexical_top = || -> Vec<PathBuf> {
            scored
                .iter()
                .take(SELECT_LIMIT)
                .map(|(p, _)| PathBuf::from(p))
                .collect()
        };
        let completion = ctx
            .router()
            .complete(
                CompleteRequest {
                    prompt: select_prompt(&goal, &scored),
                },
                RouteHints {
                    task_kind: TaskKind::Architecture,
                    ..RouteHints::default()
                },
            )
            .await?;

        let candidate_set: HashSet<&str> = scored.iter().map(|(p, _)| p.as_str()).collect();
        let selected = match extract_json::<SelectResponse>(&completion.text) {
            Ok(resp) => {
                let picked: Vec<PathBuf> = resp
                    .files
                    .into_iter()
                    .filter(|p| candidate_set.contains(p.as_str()))
                    .take(SELECT_LIMIT)
                    .map(PathBuf::from)
                    .collect();
                if picked.is_empty() {
                    lexical_top()
                } else {
                    picked
                }
            }
            Err(_) => lexical_top(),
        };

        Ok(AgentOutput::Context { files: selected })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use otto_engine_core::tool::{DenyAsk, Tool, ToolRegistry};
    use otto_engine_core::traits::Workspace;
    use otto_engine_core::types::Edit;
    use otto_providers::{LocalProvider, ScriptedProvider};
    use otto_router::SingleProviderRouter;
    use otto_tools::{DefaultPermissionGate, FsListTool, FsReadTool};
    use otto_workspace::LocalWorkspace;
    use std::sync::Arc;

    async fn seed(ws: &LocalWorkspace, path: &str, contents: &str) {
        ws.apply_edit(&Edit {
            path: PathBuf::from(path),
            new_contents: contents.to_string(),
        })
        .await
        .unwrap();
    }

    /// A registry with fs.list + fs.read over `ws_path` (both Allow for non-sensitive paths).
    fn registry(ws_path: &std::path::Path) -> ToolRegistry {
        let mut reg = ToolRegistry::new(Arc::new(DefaultPermissionGate::new()), Arc::new(DenyAsk));
        let ws: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(ws_path));
        reg.register(Arc::new(FsListTool::new(Arc::clone(&ws))) as Arc<dyn Tool>);
        reg.register(Arc::new(FsReadTool::new(ws)) as Arc<dyn Tool>);
        reg
    }

    async fn find(
        router: &SingleProviderRouter,
        ws_path: &std::path::Path,
        goal: &str,
    ) -> Vec<PathBuf> {
        let ws = LocalWorkspace::new(ws_path);
        let tools = registry(ws_path);
        let ctx = AgentCtx::new(router, &ws, &tools);
        match ContextFinder
            .run(
                AgentRequest::FindContext {
                    goal: goal.to_string(),
                },
                &ctx,
            )
            .await
            .unwrap()
        {
            AgentOutput::Context { files } => files,
            other => panic!("expected Context, got {other:?}"),
        }
    }

    #[test]
    fn keywords_drops_short_and_stopwords_and_dedupes() {
        let kws = keywords("Fix the login Login flow at io");
        assert!(kws.contains(&"login".to_string()));
        assert!(kws.contains(&"flow".to_string()));
        assert!(!kws.contains(&"fix".to_string()));
        assert!(!kws.contains(&"the".to_string()));
        assert!(!kws.contains(&"io".to_string()));
        assert_eq!(kws.iter().filter(|k| *k == "login").count(), 1);
    }

    #[test]
    fn score_weights_path_above_content() {
        let kws = keywords("login");
        let path_hit = score_file("src/login.rs", Some("nothing"), &kws);
        let content_hit = score_file("src/util.rs", Some("login login"), &kws);
        assert!(path_hit > 0 && content_hit > 0);
        assert!(path_hit > content_hit / 2);
    }

    #[tokio::test]
    async fn lexical_fallback_ranks_relevant_file_first() {
        let dir = tempfile::tempdir().unwrap();
        let ws = LocalWorkspace::new(dir.path());
        seed(&ws, "src/auth/login.rs", "fn login() {}").await;
        seed(&ws, "README.md", "totally unrelated prose").await;
        seed(&ws, "src/util.rs", "fn helper() {}").await;
        let router = SingleProviderRouter::new(Arc::new(LocalProvider::new()));
        let files = find(&router, dir.path(), "fix the login flow").await;
        assert_eq!(files.first(), Some(&PathBuf::from("src/auth/login.rs")));
        assert!(!files.contains(&PathBuf::from("README.md")));
        assert!(!files.contains(&PathBuf::from("src/util.rs")));
    }

    #[tokio::test]
    async fn llm_narrows_the_candidate_set() {
        let dir = tempfile::tempdir().unwrap();
        let ws = LocalWorkspace::new(dir.path());
        seed(&ws, "login_a.rs", "login").await;
        seed(&ws, "login_b.rs", "login").await;
        let provider = ScriptedProvider::new("{}").on("Candidates", r#"{"files": ["login_b.rs"]}"#);
        let router = SingleProviderRouter::new(Arc::new(provider));
        let files = find(&router, dir.path(), "login").await;
        assert_eq!(files, vec![PathBuf::from("login_b.rs")]);
    }

    #[tokio::test]
    async fn hallucinated_paths_are_filtered_out() {
        let dir = tempfile::tempdir().unwrap();
        let ws = LocalWorkspace::new(dir.path());
        seed(&ws, "real.rs", "login").await;
        let provider = ScriptedProvider::new("{}")
            .on("Candidates", r#"{"files": ["nonexistent.rs", "real.rs"]}"#);
        let router = SingleProviderRouter::new(Arc::new(provider));
        let files = find(&router, dir.path(), "login").await;
        assert_eq!(files, vec![PathBuf::from("real.rs")]);
    }

    #[tokio::test]
    async fn empty_workspace_returns_no_context() {
        let dir = tempfile::tempdir().unwrap();
        let router = SingleProviderRouter::new(Arc::new(LocalProvider::new()));
        let files = find(&router, dir.path(), "do something").await;
        assert!(files.is_empty());
    }
}
