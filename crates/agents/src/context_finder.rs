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
/// Maximum files whose contents are read per turn; the rest are scored on their path only. This
/// bounds per-turn read cost on large repos — small repos (fewer text files than this) read all.
const READ_BUDGET: usize = 200;

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

/// Whether a path is a binary/non-text file to skip before reading — by extension or by a known
/// lockfile name. Keeps the read budget for source files. Extensionless files (e.g. `Makefile`,
/// scripts) are kept.
fn is_skippable(path: &str) -> bool {
    const SKIP_EXTS: &[&str] = &[
        // images / media
        "png", "jpg", "jpeg", "gif", "webp", "ico", "bmp", "tiff", "mp3", "mp4", "mov", "avi",
        "wav", "ogg", "flac", "webm", // archives
        "zip", "gz", "tgz", "tar", "xz", "zst", "bz2", "7z", "rar",
        // binaries / objects
        "exe", "dll", "so", "dylib", "o", "a", "bin", "wasm", "class", "pyc", "pyo", "obj",
        // docs / fonts
        "pdf", "ttf", "otf", "woff", "woff2",
    ];
    const SKIP_NAMES: &[&str] = &[
        "Cargo.lock",
        "package-lock.json",
        "yarn.lock",
        "pnpm-lock.yaml",
        "poetry.lock",
        "Pipfile.lock",
    ];
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    if SKIP_NAMES.contains(&name) {
        return true;
    }
    match name.rsplit_once('.') {
        Some((_, ext)) => SKIP_EXTS.contains(&ext.to_lowercase().as_str()),
        None => false,
    }
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

fn select_prompt(
    goal: &str,
    candidates: &[(String, u64)],
    symbols_by_path: &std::collections::HashMap<String, Vec<String>>,
) -> String {
    let listed = candidates
        .iter()
        .map(|(p, s)| match symbols_by_path.get(p) {
            Some(syms) if !syms.is_empty() => {
                format!("- {p} (score {s}) [symbols: {}]", syms.join(", "))
            }
            _ => format!("- {p} (score {s})"),
        })
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

impl ContextFinder {
    /// The deterministic lexical candidate pipeline: enumerate via fs.list, path-score, read the
    /// top READ_BUDGET files' contents, final-score, and keep the top CANDIDATE_LIMIT. This is the
    /// offline fallback used when no retriever is wired AND when a wired retriever errors.
    async fn lexical_candidates(&self, goal: &str, ctx: &AgentCtx<'_>) -> Vec<(String, u64)> {
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
        let kws = keywords(goal);
        let mut by_path: Vec<(String, u64)> = files
            .into_iter()
            .filter(|p| !is_skippable(p))
            .map(|p| {
                let path_score = score_file(&p, None, &kws);
                (p, path_score)
            })
            .collect();
        by_path.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let read_set: HashSet<String> = by_path
            .iter()
            .take(READ_BUDGET)
            .map(|(p, _)| p.clone())
            .collect();
        let mut scored: Vec<(String, u64)> = Vec::new();
        for (path, path_score) in &by_path {
            let score = if read_set.contains(path) {
                let content = match ctx.tools().call("fs.read", json!({ "path": path })).await {
                    Ok(Value::Object(map)) => map
                        .get("content")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    _ => None,
                };
                score_file(path, content.as_deref(), &kws)
            } else {
                *path_score
            };
            if score > 0 {
                scored.push((path.clone(), score));
            }
        }
        scored.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        scored.truncate(CANDIDATE_LIMIT);
        scored
    }
}

#[async_trait]
impl Agent for ContextFinder {
    async fn run(&self, req: AgentRequest, ctx: &AgentCtx) -> anyhow::Result<AgentOutput> {
        let AgentRequest::FindContext { goal } = req else {
            anyhow::bail!("ContextFinder received a non-FindContext request");
        };

        // Produce the scored candidate set (+ matched symbol names) from the retriever when wired,
        // else the deterministic lexical pipeline. A retriever ERROR falls back to the lexical
        // pipeline — retrieval is an optimization, never a gate. The lexical path has no symbols.
        let mut symbols_by_path: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        let scored: Vec<(String, u64)> = match ctx.retriever() {
            Some(retriever) => match retriever.search(&goal, CANDIDATE_LIMIT).await {
                Ok(candidates) => candidates
                    .into_iter()
                    .map(|c| {
                        let p = c.path.to_string_lossy().into_owned();
                        if !c.symbols.is_empty() {
                            symbols_by_path.insert(p.clone(), c.symbols);
                        }
                        (p, c.score)
                    })
                    .collect(),
                Err(e) => {
                    // Retrieval is an optimization, never a gate: a search error degrades to the
                    // deterministic lexical pipeline rather than blanking out the turn's context.
                    eprintln!(
                        "warning: retriever search failed ({e}); falling back to lexical scan"
                    );
                    self.lexical_candidates(&goal, ctx).await
                }
            },
            None => self.lexical_candidates(&goal, ctx).await,
        };

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
                    prompt: select_prompt(&goal, &scored, &symbols_by_path),
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
    use otto_engine_core::traits::{Workspace, WorkspaceRead};
    use otto_engine_core::types::Edit;
    use otto_engine_core::{Candidate, Retriever};
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
        let ws: Arc<dyn WorkspaceRead> = Arc::new(LocalWorkspace::new(ws_path));
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

    #[test]
    fn is_skippable_filters_binaries_and_lockfiles() {
        assert!(is_skippable("assets/logo.png"));
        assert!(is_skippable("Cargo.lock"));
        assert!(is_skippable("dir/package-lock.json"));
        assert!(!is_skippable("src/main.rs"));
        assert!(!is_skippable("Makefile")); // extensionless is kept
        assert!(!is_skippable("README.md"));
    }

    #[tokio::test]
    async fn extension_filter_skips_binaries() {
        let dir = tempfile::tempdir().unwrap();
        let ws = LocalWorkspace::new(dir.path());
        seed(&ws, "login.png", "login login login").await;
        seed(&ws, "login.rs", "fn x() {}").await;
        let router = SingleProviderRouter::new(Arc::new(LocalProvider::new()));
        let files = find(&router, dir.path(), "login").await;
        assert!(files.contains(&PathBuf::from("login.rs")));
        assert!(
            !files.contains(&PathBuf::from("login.png")),
            "binary files are filtered out before reading: {files:?}"
        );
    }

    #[tokio::test]
    async fn content_only_match_within_budget_is_found() {
        let dir = tempfile::tempdir().unwrap();
        let ws = LocalWorkspace::new(dir.path());
        seed(&ws, "helper.rs", "fn do_login() {}").await; // 'login' in content only
        seed(&ws, "unrelated.rs", "fn nothing() {}").await;
        let router = SingleProviderRouter::new(Arc::new(LocalProvider::new()));
        let files = find(&router, dir.path(), "login").await;
        assert!(
            files.contains(&PathBuf::from("helper.rs")),
            "content-only match is found in a small repo: {files:?}"
        );
        assert!(!files.contains(&PathBuf::from("unrelated.rs")));
    }

    #[tokio::test]
    async fn content_only_match_beyond_read_budget_is_missed() {
        let dir = tempfile::tempdir().unwrap();
        let ws = LocalWorkspace::new(dir.path());
        for i in 0..READ_BUDGET {
            seed(&ws, &format!("noise/f{i:04}.txt"), "nothing relevant here").await;
        }
        seed(&ws, "zzz_only.txt", "login logic lives here").await; // content-only, sorts last
        seed(&ws, "login_handler.rs", "nothing relevant here").await; // path hit

        let router = SingleProviderRouter::new(Arc::new(LocalProvider::new()));
        let files = find(&router, dir.path(), "login").await;
        assert!(
            files.contains(&PathBuf::from("login_handler.rs")),
            "a path-hit file is found regardless of budget: {files:?}"
        );
        assert!(
            !files.contains(&PathBuf::from("zzz_only.txt")),
            "a content-only match beyond the read budget is missed: {files:?}"
        );
    }

    struct StubRetriever(Vec<Candidate>);
    #[async_trait::async_trait]
    impl Retriever for StubRetriever {
        async fn search(&self, _goal: &str, _limit: usize) -> anyhow::Result<Vec<Candidate>> {
            Ok(self.0.clone())
        }
    }

    struct FailingRetriever;
    #[async_trait::async_trait]
    impl Retriever for FailingRetriever {
        async fn search(&self, _goal: &str, _limit: usize) -> anyhow::Result<Vec<Candidate>> {
            anyhow::bail!("simulated index failure")
        }
    }

    #[tokio::test]
    async fn retriever_candidates_supersede_the_lexical_scan() {
        let dir = tempfile::tempdir().unwrap();
        let ws = LocalWorkspace::new(dir.path());
        // The lexical pipeline would rank `auth.rs` first (path + content hit on "login")
        // and would DROP `from_retriever.rs` entirely (no "login" match → score 0).
        seed(&ws, "auth.rs", "fn login() {}").await;
        seed(&ws, "from_retriever.rs", "totally unrelated prose").await;
        let tools = registry(dir.path());
        let router = SingleProviderRouter::new(Arc::new(LocalProvider::new()));
        // Retriever returns ONLY the file the lexical scan would never pick.
        let retriever = StubRetriever(vec![Candidate {
            path: PathBuf::from("from_retriever.rs"),
            score: 99,
            symbols: Vec::new(),
        }]);
        let ctx = AgentCtx::new(&router, &ws, &tools).with_retriever(&retriever);
        let out = ContextFinder
            .run(
                AgentRequest::FindContext {
                    goal: "login".into(),
                },
                &ctx,
            )
            .await
            .unwrap();
        let AgentOutput::Context { files } = out else {
            panic!("expected Context")
        };
        // Proves candidates came from the retriever, not the lexical scan:
        // `from_retriever.rs` is present, and the lexically-superior `auth.rs` is absent.
        assert_eq!(files, vec![PathBuf::from("from_retriever.rs")]);
        assert!(!files.contains(&PathBuf::from("auth.rs")));
    }

    #[tokio::test]
    async fn retriever_error_falls_back_to_lexical_not_empty() {
        let dir = tempfile::tempdir().unwrap();
        let ws = LocalWorkspace::new(dir.path());
        seed(&ws, "auth.rs", "fn login() {}").await; // lexical would find this on "login"
        let tools = registry(dir.path());
        let router = SingleProviderRouter::new(Arc::new(LocalProvider::new()));
        let retriever = FailingRetriever;
        let ctx = AgentCtx::new(&router, &ws, &tools).with_retriever(&retriever);
        let out = ContextFinder
            .run(
                AgentRequest::FindContext {
                    goal: "login".into(),
                },
                &ctx,
            )
            .await
            .unwrap();
        let AgentOutput::Context { files } = out else {
            panic!("expected Context")
        };
        assert!(
            files.contains(&PathBuf::from("auth.rs")),
            "a retriever error must degrade to the lexical scan, not empty context: {files:?}"
        );
    }

    #[test]
    fn select_prompt_lists_symbols_only_when_present() {
        let cands = vec![("a.rs".to_string(), 10u64), ("b.rs".to_string(), 5u64)];
        let mut syms = std::collections::HashMap::new();
        syms.insert(
            "a.rs".to_string(),
            vec!["login".to_string(), "logout".to_string()],
        );
        let p = select_prompt("goal", &cands, &syms);
        assert!(
            p.contains("- a.rs (score 10) [symbols: login, logout]"),
            "{p}"
        );
        assert!(p.contains("- b.rs (score 5)"), "{p}");
        assert!(!p.contains("- b.rs (score 5) [symbols"), "{p}");
    }

    #[tokio::test]
    async fn select_prompt_symbols_reach_the_model() {
        let dir = tempfile::tempdir().unwrap();
        let ws = LocalWorkspace::new(dir.path());
        seed(&ws, "a.rs", "fn x() {}").await;
        seed(&ws, "b.rs", "fn y() {}").await;
        let tools = registry(dir.path());
        // The retriever ranks a.rs first; only a.rs carries a symbol. The scripted provider picks
        // b.rs ONLY if the prompt contained "[symbols: alpha]" — distinguishing it from the
        // lexical-top fallback (which would be [a.rs, b.rs]).
        let retriever = StubRetriever(vec![
            Candidate {
                path: PathBuf::from("a.rs"),
                score: 10,
                symbols: vec!["alpha".to_string()],
            },
            Candidate {
                path: PathBuf::from("b.rs"),
                score: 5,
                symbols: Vec::new(),
            },
        ]);
        let provider = ScriptedProvider::new("{}").on("[symbols: alpha]", r#"{"files": ["b.rs"]}"#);
        let router = SingleProviderRouter::new(Arc::new(provider));
        let ctx = AgentCtx::new(&router, &ws, &tools).with_retriever(&retriever);
        let out = ContextFinder
            .run(
                AgentRequest::FindContext {
                    goal: "anything".into(),
                },
                &ctx,
            )
            .await
            .unwrap();
        let AgentOutput::Context { files } = out else {
            panic!("expected Context")
        };
        assert_eq!(
            files,
            vec![PathBuf::from("b.rs")],
            "symbol-enriched prompt drove the pick"
        );
    }
}
