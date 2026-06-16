# otto Plan 4d — Real ContextFinder (+ Coder reads contents) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace `StubContextFinder` — the last stub in the spine — with a real hybrid ContextFinder (deterministic lexical prefilter → LLM rank, with lexical fallback), and make the `Coder` read the selected files' contents into its prompt.

**Architecture:** `LocalWorkspace::list` gains recursive enumeration (a `**` glob) with a fixed ignore-list. The new `ContextFinder` lists the workspace, scores files lexically against goal keywords, takes the top candidates, asks the model to pick the most relevant subset (falling back to the lexical top-N when the model doesn't answer in schema — which keeps the default `LocalProvider` path deterministic), and returns ranked `Vec<PathBuf>`. The `Coder` reads those files via the gated `fs.read` tool (budgeted) and embeds their contents in its prompt. The trait seam (`Context`/`Code` payloads) is unchanged.

**Tech Stack:** Rust (edition 2024), serde, async-trait, anyhow, tokio, serde_json, tempfile (dev). Providers: `LocalProvider` (deterministic) + `ScriptedProvider` (tests).

---

## Context for the implementer (read once)

- Agents implement `Agent::run(&self, req: AgentRequest, ctx: &AgentCtx) -> anyhow::Result<AgentOutput>`. Use `ctx: &AgentCtx` (never `<'_>`).
- `ctx.router().complete(CompleteRequest { prompt }, RouteHints { task_kind, prior_failures, ..RouteHints::default() })` calls the model. `ctx.tools().call(name, json_args) -> anyhow::Result<Value>` calls a tool.
- Tools are MCP-shaped: `fs.list` returns `{"paths": [..]}`; `fs.read` returns `{"content": "<utf8>"}` and **errors on non-UTF8** (so a binary file → `Err` → treat as unreadable); `bash` returns `{stdout,stderr,exit_code}`.
- `extract_json::<T>(text) -> anyhow::Result<T>` (in `crates/agents/src/parse.rs`) tolerates ```code fences``` and surrounding prose. Mirror the Planner/Coder prompt-and-parse-with-fallback pattern.
- `ScriptedProvider::new("<default>").on("<needle>", "<response>")` returns `<response>` for the first prompt containing `<needle>`, else `<default>`. `LocalProvider` echoes the prompt (never valid JSON) — this is what triggers the deterministic fallback paths.
- The permission gate classifies `fs.read`/`fs.list` as `Allow` for non-sensitive paths, so a `DenyAsk` resolver is fine for them in tests; the sensitive-path floor (`.env`/`.ssh`/`.git`/`.aws`) always denies.
- `LocalWorkspace::apply_edit` creates parent dirs, so seeding `src/foo.rs` in a test just works.
- Conventions: stay on branch `feat/plan-4d-contextfinder`; never detach HEAD; `git add`+`commit` only (no `--amend`); per-package then workspace gates; `cargo clippy --all-targets -- -D warnings` clean; `cargo fmt` clean; TDD; **no AI/Claude self-attribution anywhere**.

---

## File Structure

```
crates/workspace/src/lib.rs           # MODIFY: recursive glob support in `list` + ignore-list
crates/agents/src/context_finder.rs   # NEW: hybrid ContextFinder (lexical -> LLM, fallback)
crates/agents/src/coder.rs            # MODIFY: read context files, inject path+contents (budgeted)
crates/agents/src/lib.rs              # MODIFY: declare/re-export ContextFinder; remove StubContextFinder
crates/engine/src/lib.rs             # MODIFY: register the real ContextFinder
crates/engine/tests/context.rs        # NEW: end-to-end context-flows-to-Coder integration test
docs/ARCHITECTURE.md                  # MODIFY: document the real ContextFinder + recursive list
```

---

## Task 1: Recursive `fs.list` in `LocalWorkspace::list`

**Files:**
- Modify: `crates/workspace/src/lib.rs`

The current `list` ignores its glob and lists the root shallowly. Add a recursive mode triggered by a `**` glob, preserving the existing shallow behavior for any other glob.

- [ ] **Step 1: Write the failing tests**

Add to the `#[cfg(test)] mod tests` in `crates/workspace/src/lib.rs`:

```rust
    #[tokio::test]
    async fn recursive_list_walks_subdirs_and_skips_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let ws = LocalWorkspace::new(dir.path());
        for (p, c) in [
            ("src/lib.rs", "a"),
            ("src/inner/mod.rs", "b"),
            ("target/debug/junk.rs", "c"),
            (".git/config", "d"),
            ("node_modules/x/index.js", "e"),
        ] {
            ws.apply_edit(&Edit {
                path: PathBuf::from(p),
                new_contents: c.to_string(),
            })
            .await
            .unwrap();
        }
        let listing = ws.list("**").await.unwrap();
        assert!(listing.contains(&PathBuf::from("src/lib.rs")));
        assert!(listing.contains(&PathBuf::from("src/inner/mod.rs")));
        // Ignored directories are skipped entirely.
        assert!(!listing.iter().any(|p| p.starts_with("target")));
        assert!(!listing.iter().any(|p| p.starts_with(".git")));
        assert!(!listing.iter().any(|p| p.starts_with("node_modules")));
        // Recursive mode returns files only (no bare directory entries).
        assert!(!listing.contains(&PathBuf::from("src")));
        // Deterministic order.
        let mut sorted = listing.clone();
        sorted.sort();
        assert_eq!(listing, sorted);
    }

    #[tokio::test]
    async fn shallow_list_unchanged_for_star_glob() {
        let dir = tempfile::tempdir().unwrap();
        let ws = LocalWorkspace::new(dir.path());
        ws.apply_edit(&Edit {
            path: PathBuf::from("a.txt"),
            new_contents: "a".to_string(),
        })
        .await
        .unwrap();
        ws.apply_edit(&Edit {
            path: PathBuf::from("sub/b.txt"),
            new_contents: "b".to_string(),
        })
        .await
        .unwrap();
        // Shallow mode lists only top-level entries (the existing behavior).
        let listing = ws.list("*").await.unwrap();
        assert!(listing.contains(&PathBuf::from("a.txt")));
        assert!(listing.contains(&PathBuf::from("sub")));
        assert!(!listing.contains(&PathBuf::from("sub/b.txt")));
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p otto-workspace recursive_list_walks_subdirs_and_skips_ignored shallow_list_unchanged_for_star_glob`
Expected: FAIL (`recursive_list...` fails — current `list` is shallow and ignores the glob).

- [ ] **Step 3: Implement recursive listing**

Replace the existing `list` method in `crates/workspace/src/lib.rs` (the one with signature `async fn list(&self, _glob: &str) -> anyhow::Result<Vec<PathBuf>>`) with:

```rust
    async fn list(&self, glob: &str) -> anyhow::Result<Vec<PathBuf>> {
        // Shallow mode (the default `*`): list the root's immediate entries, unchanged.
        if !glob.contains("**") {
            let mut entries = tokio::fs::read_dir(&self.root).await?;
            let mut out = Vec::new();
            while let Some(entry) = entries.next_entry().await? {
                if let Ok(rel) = entry.path().strip_prefix(&self.root) {
                    out.push(rel.to_path_buf());
                }
            }
            out.sort();
            return Ok(out);
        }

        // Recursive mode (`**`): walk the subtree, returning files only. Skips a fixed set of
        // ignored directories (build/VCS/dependency dirs and any dotfile/dotdir, which also
        // covers the gate's sensitive-path floor), does not follow symlinks, and caps the
        // number of files to bound cost. Output is sorted for determinism.
        const MAX_ENTRIES: usize = 5000;
        fn ignored(name: &str) -> bool {
            name == ".git"
                || name == "target"
                || name == "node_modules"
                || name.starts_with('.')
        }
        let mut out = Vec::new();
        let mut stack = vec![self.root.clone()];
        while let Some(dir) = stack.pop() {
            let Ok(mut entries) = tokio::fs::read_dir(&dir).await else {
                continue; // skip directories we cannot read rather than failing the whole walk
            };
            while let Some(entry) = entries.next_entry().await? {
                let file_type = entry.file_type().await?;
                if file_type.is_symlink() {
                    continue;
                }
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if ignored(&name) {
                    continue;
                }
                let path = entry.path();
                if file_type.is_dir() {
                    stack.push(path);
                } else if file_type.is_file() {
                    if let Ok(rel) = path.strip_prefix(&self.root) {
                        out.push(rel.to_path_buf());
                        if out.len() >= MAX_ENTRIES {
                            out.sort();
                            return Ok(out);
                        }
                    }
                }
            }
        }
        out.sort();
        Ok(out)
    }
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p otto-workspace`
Expected: PASS (the two new tests + the existing `list_returns_relative_entries` and others).

- [ ] **Step 5: Lint, format, commit**

Run: `cargo clippy -p otto-workspace --all-targets -- -D warnings` (clean) and `cargo fmt -p otto-workspace`.

```bash
git add crates/workspace/src/lib.rs
git commit -m "feat(workspace): recursive list via ** glob with an ignore-list"
```

---

## Task 2: Real `ContextFinder` (hybrid lexical → LLM)

**Files:**
- Create: `crates/agents/src/context_finder.rs`
- Modify: `crates/agents/src/lib.rs`

- [ ] **Step 1: Write `context_finder.rs` with its tests**

Create `crates/agents/src/context_finder.rs`:

```rust
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
                Ok(Value::Object(map)) => {
                    map.get("content").and_then(Value::as_str).map(str::to_string)
                }
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
                    .filter(|p| candidate_set.contains(p.as_str())) // reject hallucinated paths
                    .take(SELECT_LIMIT)
                    .map(PathBuf::from)
                    .collect();
                if picked.is_empty() { lexical_top() } else { picked }
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

    async fn find(router: &SingleProviderRouter, ws_path: &std::path::Path, goal: &str) -> Vec<PathBuf> {
        let ws = LocalWorkspace::new(ws_path);
        let tools = registry(ws_path);
        let ctx = AgentCtx::new(router, &ws, &tools);
        match ContextFinder
            .run(AgentRequest::FindContext { goal: goal.to_string() }, &ctx)
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
        assert!(!kws.contains(&"fix".to_string())); // stopword
        assert!(!kws.contains(&"the".to_string())); // stopword
        assert!(!kws.contains(&"io".to_string())); // < 3 chars
        assert_eq!(kws.iter().filter(|k| *k == "login").count(), 1); // de-duped
    }

    #[test]
    fn score_weights_path_above_content() {
        let kws = keywords("login");
        let path_hit = score_file("src/login.rs", Some("nothing"), &kws);
        let content_hit = score_file("src/util.rs", Some("login login"), &kws);
        assert!(path_hit > 0 && content_hit > 0);
        assert!(path_hit > content_hit / 2); // a single path hit (5) beats two content hits (2)
    }

    #[tokio::test]
    async fn lexical_fallback_ranks_relevant_file_first() {
        // LocalProvider never returns JSON, so selection falls back to the lexical top-N.
        let dir = tempfile::tempdir().unwrap();
        let ws = LocalWorkspace::new(dir.path());
        seed(&ws, "src/auth/login.rs", "fn login() {}").await;
        seed(&ws, "README.md", "totally unrelated prose").await;
        seed(&ws, "src/util.rs", "fn helper() {}").await;
        let router = SingleProviderRouter::new(Arc::new(LocalProvider::new()));
        let files = find(&router, dir.path(), "fix the login flow").await;
        assert_eq!(files.first(), Some(&PathBuf::from("src/auth/login.rs")));
        // Zero-scoring files are excluded.
        assert!(!files.contains(&PathBuf::from("README.md")));
        assert!(!files.contains(&PathBuf::from("src/util.rs")));
    }

    #[tokio::test]
    async fn llm_narrows_the_candidate_set() {
        let dir = tempfile::tempdir().unwrap();
        let ws = LocalWorkspace::new(dir.path());
        seed(&ws, "login_a.rs", "login").await;
        seed(&ws, "login_b.rs", "login").await;
        // The select prompt contains "Candidates:"; the model narrows to login_b.rs only.
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
```

- [ ] **Step 2: Declare and re-export the module**

In `crates/agents/src/lib.rs`, add to the module declarations and re-exports (next to `coder`/`planner`/`verifier`):

```rust
pub mod context_finder;
```
```rust
pub use context_finder::ContextFinder;
```

(Do NOT remove `StubContextFinder` yet — Task 3's Coder change and Task 4's wiring still build against the current engine. Removing the stub happens in Task 4, together with the engine swap, so the workspace never has a dangling reference. `StubContextFinder` and `ContextFinder` coexisting for one task is fine.)

- [ ] **Step 3: Run the tests**

Run: `cargo test -p otto-agents context_finder::`
Expected: PASS (7 tests). Then `cargo clippy -p otto-agents --all-targets -- -D warnings` (clean) and `cargo fmt -p otto-agents`.

- [ ] **Step 4: Commit**

```bash
git add crates/agents/src/context_finder.rs crates/agents/src/lib.rs
git commit -m "feat(agents): real ContextFinder — lexical prefilter + LLM rank with fallback"
```

---

## Task 3: Coder reads context file contents

**Files:**
- Modify: `crates/agents/src/coder.rs`

The Coder currently lists only file *names*. Make it read each context file via `fs.read` and embed `path + contents` blocks, budgeted to bound prompt size.

- [ ] **Step 1: Write the failing tests**

Add these to the `#[cfg(test)] mod tests` in `crates/agents/src/coder.rs`. Also add the imports they need at the top of the test module (`Tool`, `FsReadTool`, `Workspace`, `Edit`):

```rust
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

    #[test]
    fn truncate_chars_caps_and_marks() {
        let s: String = "x".repeat(100);
        let t = truncate_chars(&s, 10);
        assert!(t.starts_with(&"x".repeat(10)));
        assert!(t.contains("truncated"));
        // Short input is returned unchanged.
        assert_eq!(truncate_chars("short", 10), "short");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p otto-agents coder::`
Expected: FAIL (`truncate_chars` not defined; `reads_context_file_contents_into_prompt` fails because the current Coder only embeds the path name `src/lib.rs`, not the contents `special_marker_42`).

- [ ] **Step 3: Implement reading + budgeted injection**

In `crates/agents/src/coder.rs`:

(a) Add `use serde_json::{Value, json};` to the imports.

(b) Add these budget constants and the truncation helper above `code_prompt`:

```rust
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
            Ok(Value::Object(map)) => map.get("content").and_then(Value::as_str).map(str::to_string),
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
```

(c) Replace the `code_prompt` function with a version that takes the read files and embeds contents:

```rust
fn code_prompt(goal: &str, files: &[(PathBuf, String)], feedback: Option<&str>) -> String {
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
```

(d) In `impl Agent for Coder`, read the context before building the prompt. Replace the `prompt: code_prompt(&goal, &context, feedback.as_deref()),` line with a prior read step. The relevant region becomes:

```rust
        let files = read_context(ctx, &context).await;
        let completion = ctx
            .router()
            .complete(
                CompleteRequest {
                    prompt: code_prompt(&goal, &files, feedback.as_deref()),
                },
                RouteHints {
                    task_kind: TaskKind::Edit,
                    prior_failures,
                    ..RouteHints::default()
                },
            )
            .await?;
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p otto-agents coder::`
Expected: PASS — the new two tests plus the existing `parses_edits_from_json`, `falls_back_to_no_edits_when_unparseable`, and `feedback_is_included_in_the_prompt` (those use an empty `context`, so `read_context` returns empty, the prompt shows `(none)`, and their `edits`/`MARKER-9F3` needles still match).

- [ ] **Step 5: Lint, format, commit**

Run: `cargo clippy -p otto-agents --all-targets -- -D warnings` (clean) and `cargo fmt -p otto-agents`.

```bash
git add crates/agents/src/coder.rs
git commit -m "feat(agents): Coder reads context file contents into its prompt (budgeted)"
```

---

## Task 4: Wire the real ContextFinder; remove the stub; end-to-end test

**Files:**
- Modify: `crates/engine/src/lib.rs`
- Modify: `crates/agents/src/lib.rs`
- Create: `crates/engine/tests/context.rs`

- [ ] **Step 1: Swap the registration and remove the stub**

In `crates/engine/src/lib.rs`:
- Change the import `use otto_agents::{Coder, Planner, StubContextFinder, Verifier};` to:
```rust
use otto_agents::{Coder, ContextFinder, Planner, Verifier};
```
- Change the registration line to:
```rust
    registry.register(Role::ContextFinder, Arc::new(ContextFinder));
```
- Update the `build_default_registry` doc comment that says "ContextFinder remains a stub." to: "the whole spine is real: LLM-backed Planner + ContextFinder + Coder and a cargo-check Verifier. No stubs remain."

In `crates/agents/src/lib.rs`:
- DELETE the `StubContextFinder` struct and its entire `impl Agent` block.
- Update the crate-level `//!` doc: change the line "Only `StubContextFinder` remains a stub until its real version lands." to "All four spine agents (`Planner`, `ContextFinder`, `Coder`, `Verifier`) are real."
- After removing `StubContextFinder`, run clippy and remove any import in `lib.rs` that is now unused (the stub's `Agent`/`AgentCtx`/`AgentOutput`/`AgentRequest`/`Value`/`async_trait` imports may become dead now that no agent is defined directly in `lib.rs`; delete whatever clippy flags).

- [ ] **Step 2: Add the end-to-end context-flow integration test**

Create `crates/engine/tests/context.rs`:

```rust
//! End-to-end: the ContextFinder selects a seeded file and the Coder reads its contents, so the
//! scripted model only produces the edit when the file's contents reached the Coder's prompt.

use std::path::Path;
use std::sync::Arc;

use otto_engine::{build_tool_registry, run_goal};
use otto_engine_core::traits::Workspace;
use otto_engine_core::types::Edit;
use otto_providers::ScriptedProvider;
use otto_router::SingleProviderRouter;
use otto_workspace::LocalWorkspace;

#[tokio::test]
async fn context_flows_from_finder_to_coder() {
    let dir = tempfile::tempdir().unwrap();
    let seed_ws = LocalWorkspace::new(dir.path());
    // Seed a file the goal's keywords match ("thing"), containing a unique marker.
    seed_ws
        .apply_edit(&Edit {
            path: std::path::PathBuf::from("src/thing.rs"),
            new_contents: "fn thing() { /* CTX_MARKER_77 */ }".to_string(),
        })
        .await
        .unwrap();

    // The coder rule fires only on the seeded file's marker — proving the ContextFinder picked
    // src/thing.rs and the Coder injected its contents. The context-finder's own select prompt
    // contains neither "CTX_MARKER_77" nor "edits", so it falls back to the lexical pick.
    let provider = ScriptedProvider::new("{}").on(
        "CTX_MARKER_77",
        r#"{"edits": [{"path": "result.txt", "contents": "used context"}]}"#,
    );
    let router = SingleProviderRouter::new(Arc::new(provider));
    let workspace = LocalWorkspace::new(dir.path());

    let tools_workspace: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(dir.path()));
    let tools = build_tool_registry(tools_workspace, dir.path().to_path_buf());

    let (_events, outcome) = run_goal("update the thing function", &router, &workspace, &tools)
        .await
        .unwrap();

    assert!(outcome.ok);
    let written = workspace.read(Path::new("result.txt")).await.unwrap();
    assert_eq!(String::from_utf8(written).unwrap(), "used context");
}
```

- [ ] **Step 3: Full workspace test + gates**

Run: `cargo test --workspace`. ALL pass, including:
- The new `context_flows_from_finder_to_coder`.
- The existing `full_turn_writes_parsed_edit_and_completes_ok` — its workspace is empty when the ContextFinder runs, so the recursive list returns nothing, the ContextFinder returns empty context (no LLM call), the Coder sees `(none)`, and the `edits` rule still fires exactly as before.

Run: `cargo clippy --workspace --all-targets -- -D warnings` (clean) and `cargo fmt --all -- --check` (clean).

- [ ] **Step 4: CLI smoke (offline, deterministic)**

Run: `mkdir -p /tmp/otto-p4d && cargo run -p otto-engine -- run "add a greeting" --root /tmp/otto-p4d`
Expected: the event stream runs Planner → ContextFinder → Coder → Verifier and ends `turn ok = true` (offline: ContextFinder finds an empty/irrelevant dir → empty context, Coder falls back to no edits, Verifier finds no Cargo project → ok). Confirm no error.

- [ ] **Step 5: Commit**

```bash
git add crates/engine/src/lib.rs crates/agents/src/lib.rs crates/engine/tests/context.rs
git commit -m "feat(engine): register the real ContextFinder; spine has no stubs left"
```

---

## Task 5: Docs + final quality gate

**Files:**
- Modify: `docs/ARCHITECTURE.md`

- [ ] **Step 1: Document the real ContextFinder**

In `docs/ARCHITECTURE.md`, in the `### \`Agent\`` subsection (where the real Planner/Coder/Verifier are described), append:

```markdown
The `ContextFinder` is real: it enumerates the workspace recursively (the `fs.list` `**` glob,
which skips `.git`/`target`/`node_modules`/dotfiles and does not follow symlinks), scores files
lexically against goal keywords (path matches weighted above content matches), keeps the top
candidates, and asks the model to pick the most relevant subset — falling back to the lexical
top-N when the model does not answer in schema, so the default offline path stays deterministic.
The `Coder` then reads those files via the gated `fs.read` tool and embeds their contents
(budgeted: at most 8 files, ~8 KB each, ~32 KB total) in its prompt, so edits are grounded in
real file contents. With this, the whole spine — Planner → ContextFinder → Coder → Verifier — is
real, with no stubs remaining.
```

If a nearby sentence still says the ContextFinder is a stub, update it for consistency.

- [ ] **Step 2: Final gate**

Run: `cargo fmt --all -- --check` (clean), `cargo clippy --workspace --all-targets -- -D warnings` (clean), `cargo test --workspace` — capture the per-crate breakdown + summed total.

- [ ] **Step 3: Commit**

```bash
git add docs/ARCHITECTURE.md
git commit -m "docs: document the real ContextFinder and recursive list"
```

---

## Done — what Plan 4d delivers

The spine is fully real: **Planner → ContextFinder → Coder → Verifier**, no stubs. The
ContextFinder finds the files that matter (lexically, refined by the model when one is available)
and the Coder reads them, so edits are grounded in actual file contents — while the default
offline path stays fully deterministic.

**Carried forward / deferred:**
- Arbitrary glob-pattern matching (only recursive-vs-shallow today) and `.gitignore` parsing.
- Embedding/semantic retrieval (lexical + LLM ranking only).
- Reading every candidate file fully for scoring is acceptable at skeleton scale but a future
  optimization (e.g. path-prefilter before content reads) may be warranted on large repos.
- Verifier project types beyond Cargo; Planner milestones threaded into the Coder; a read-only
  workspace view for untrusted agents.
```
