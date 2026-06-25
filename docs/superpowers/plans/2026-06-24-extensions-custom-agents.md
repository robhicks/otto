# Extensions Custom Agents Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship slice 1 of the `extensions` crate: discover `.claude/agents/*.md` (project + user-global), parse Claude-Code-exact frontmatter, and dispatch a discovered custom agent as a subagent via a `task` tool, exposed through `otto run --agent <name> "<goal>"`.

**Architecture:** A new leaf crate `extensions` (depends inward on `engine-core`/`protocol`) holds the parser, discovery, a `MarkdownAgent` (`Agent` impl that runs its markdown body as a system prompt through the router), and a `TaskTool` (`Tool` impl that runs a named custom agent as a depth-1 sub-turn with an allowlist-filtered tool view). `engine-core` gains additive `AgentRequest::Task`/`AgentOutput::Task` variants and a `ToolRegistry::subset` constructor. The `engine` binary wires discovery + dispatch into a new `otto run --agent` path. The orchestrator spine and the offline determinism suite are untouched (discovery is invoked only by the binary).

**Tech Stack:** Rust (edition 2024), `async-trait`, `anyhow`, `serde_json`, `tokio`, `tempfile` (tests). No new third-party deps — frontmatter is hand-parsed (no YAML crate).

**Spec:** `docs/superpowers/specs/2026-06-24-extensions-custom-agents-design.md`

---

## File Structure

- `crates/extensions/Cargo.toml` — new crate manifest.
- `crates/extensions/src/lib.rs` — crate root; re-exports; `Extensions` struct; `discover()`.
- `crates/extensions/src/agent_def.rs` — `CustomAgentDef` + `parse_agent_md()`.
- `crates/extensions/src/markdown_agent.rs` — `MarkdownAgent` (`Agent` impl).
- `crates/extensions/src/task_tool.rs` — `TaskTool` (`Tool` impl).
- `crates/engine-core/src/types.rs` — add `AgentRequest::Task` / `AgentOutput::Task`.
- `crates/engine-core/src/tool.rs` — add `ToolRegistry::subset`.
- `Cargo.toml` (root) — add `crates/extensions` to workspace members.
- `crates/engine/Cargo.toml` — add `otto-extensions` dependency.
- `crates/engine/src/main.rs` — `--agent` flag in `cmd_run`.

---

## Task 1: `extensions` crate scaffold + `CustomAgentDef` + `parse_agent_md`

**Files:**
- Create: `crates/extensions/Cargo.toml`
- Create: `crates/extensions/src/lib.rs`
- Create: `crates/extensions/src/agent_def.rs`
- Modify: `Cargo.toml` (root, workspace members)

- [ ] **Step 1: Add the crate to the workspace**

In root `Cargo.toml`, add the new member to the `members` list (after `"crates/retrieval",`):

```toml
    "crates/retrieval",
    "crates/extensions",
```

- [ ] **Step 2: Create the crate manifest**

Create `crates/extensions/Cargo.toml`:

```toml
[package]
name = "otto-extensions"
version = "0.1.0"
edition.workspace = true
rust-version.workspace = true
license.workspace = true

[dependencies]
otto-engine-core = { path = "../engine-core" }
otto-protocol = { path = "../protocol" }
anyhow.workspace = true
async-trait.workspace = true
serde_json.workspace = true

[dev-dependencies]
tempfile.workspace = true
tokio = { workspace = true, features = ["macros", "rt-multi-thread"] }
```

- [ ] **Step 3: Create the crate root with module wiring**

Create `crates/extensions/src/lib.rs`:

```rust
//! Loads otto's native extension format — Claude Code's `.claude/` convention — and
//! registers each artifact into an existing otto primitive. Slice 1: custom agents.
//!
//! This crate is a leaf: it depends inward on `engine-core`/`protocol` and is wired only
//! by the `engine` binary, never by `engine-core`. The orchestrator core never calls
//! discovery, so the offline determinism suite is unaffected.

mod agent_def;

pub use agent_def::{CustomAgentDef, parse_agent_md};
```

- [ ] **Step 4: Write the failing parser test**

Create `crates/extensions/src/agent_def.rs` with the type, a stub, and tests:

```rust
//! A discovered `agents/*.md`: Claude-Code-exact YAML-ish frontmatter + a markdown body
//! that becomes the agent's system prompt.

/// One parsed custom agent. `tools = None` means "all available tools"; `Some(list)` is an
/// allowlist. `model` is preserved for a later slice; it does not influence routing yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomAgentDef {
    pub name: String,
    pub description: String,
    pub tools: Option<Vec<String>>,
    pub model: Option<String>,
    pub system_prompt: String,
}

/// Parse one `agents/*.md`. Errors if the frontmatter is absent/unterminated or is missing
/// `name`/`description`. `tools` accepts a comma-separated string or an inline `[a, b]` list.
pub fn parse_agent_md(_text: &str) -> anyhow::Result<CustomAgentDef> {
    anyhow::bail!("not implemented")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "---\nname: reviewer\ndescription: Reviews code\ntools: fs.read, grep\nmodel: claude-opus-4-8\n---\nYou are a careful code reviewer.\n";

    #[test]
    fn parses_frontmatter_and_body() {
        let def = parse_agent_md(SAMPLE).unwrap();
        assert_eq!(def.name, "reviewer");
        assert_eq!(def.description, "Reviews code");
        assert_eq!(def.tools, Some(vec!["fs.read".to_string(), "grep".to_string()]));
        assert_eq!(def.model, Some("claude-opus-4-8".to_string()));
        assert_eq!(def.system_prompt.trim(), "You are a careful code reviewer.");
    }

    #[test]
    fn tools_inline_list_form() {
        let text = "---\nname: a\ndescription: d\ntools: [fs.read, bash]\n---\nbody\n";
        let def = parse_agent_md(text).unwrap();
        assert_eq!(def.tools, Some(vec!["fs.read".to_string(), "bash".to_string()]));
    }

    #[test]
    fn omitted_tools_and_model_are_none() {
        let text = "---\nname: a\ndescription: d\n---\nbody\n";
        let def = parse_agent_md(text).unwrap();
        assert_eq!(def.tools, None);
        assert_eq!(def.model, None);
    }

    #[test]
    fn missing_name_errors() {
        let text = "---\ndescription: d\n---\nbody\n";
        assert!(parse_agent_md(text).is_err());
    }

    #[test]
    fn missing_frontmatter_errors() {
        assert!(parse_agent_md("just a body, no frontmatter").is_err());
    }
}
```

- [ ] **Step 5: Run the tests to verify they fail**

Run: `cargo test -p otto-extensions agent_def::`
Expected: FAIL — all tests fail with "not implemented".

- [ ] **Step 6: Implement `parse_agent_md`**

Replace the stub body in `crates/extensions/src/agent_def.rs`:

```rust
pub fn parse_agent_md(text: &str) -> anyhow::Result<CustomAgentDef> {
    let rest = text
        .strip_prefix("---")
        .ok_or_else(|| anyhow::anyhow!("missing frontmatter (no leading `---`)"))?;
    let end = rest
        .find("\n---")
        .ok_or_else(|| anyhow::anyhow!("unterminated frontmatter (no closing `---`)"))?;
    let front = &rest[..end];
    let body = rest[end + 4..].trim_start_matches(['\n', '\r']).to_string();

    let mut name = None;
    let mut description = None;
    let mut tools = None;
    let mut model = None;

    for line in front.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (key, value) = line
            .split_once(':')
            .ok_or_else(|| anyhow::anyhow!("malformed frontmatter line: {line}"))?;
        let value = value.trim();
        match key.trim() {
            "name" => name = Some(value.to_string()),
            "description" => description = Some(value.to_string()),
            "model" if !value.is_empty() => model = Some(value.to_string()),
            "tools" => {
                let list: Vec<String> = value
                    .trim_matches(['[', ']'])
                    .split(',')
                    .map(|s| s.trim().trim_matches(['"', '\'']).to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                if !list.is_empty() {
                    tools = Some(list);
                }
            }
            _ => {}
        }
    }

    Ok(CustomAgentDef {
        name: name.ok_or_else(|| anyhow::anyhow!("frontmatter missing `name`"))?,
        description: description.ok_or_else(|| anyhow::anyhow!("frontmatter missing `description`"))?,
        tools,
        model,
        system_prompt: body,
    })
}
```

- [ ] **Step 7: Run the tests to verify they pass**

Run: `cargo test -p otto-extensions agent_def::`
Expected: PASS — 5 tests pass.

- [ ] **Step 8: Commit**

```bash
git add Cargo.toml crates/extensions/Cargo.toml crates/extensions/src/lib.rs crates/extensions/src/agent_def.rs
git commit -m "feat(extensions): crate scaffold + agent frontmatter parser"
```

---

## Task 2: `discover()` over project + user-global

**Files:**
- Modify: `crates/extensions/src/lib.rs`

- [ ] **Step 1: Write the failing discovery test**

Append to `crates/extensions/src/lib.rs` (after the `pub use`), add the `Extensions` type, a `discover` stub, and tests:

```rust
use std::path::Path;

/// Everything discovered from the `.claude/` directories. Slice 1: custom agents only.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Extensions {
    pub agents: Vec<CustomAgentDef>,
}

/// Discover `<home>/.claude/agents/*.md` then `<project_root>/.claude/agents/*.md`. Project
/// agents override user agents of the same `name`. Malformed files are skipped (never fatal).
/// Missing directories yield no agents. `home` is explicit (never read ambiently) so callers
/// and tests stay hermetic.
pub fn discover(_project_root: &Path, _home: &Path) -> Extensions {
    Extensions::default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn write_agent(dir: &Path, file: &str, body: &str) {
        let agents = dir.join(".claude").join("agents");
        fs::create_dir_all(&agents).unwrap();
        fs::write(agents.join(file), body).unwrap();
    }

    #[test]
    fn discovers_from_both_roots() {
        let home = tempdir().unwrap();
        let proj = tempdir().unwrap();
        write_agent(home.path(), "u.md", "---\nname: u\ndescription: user agent\n---\nbody\n");
        write_agent(proj.path(), "p.md", "---\nname: p\ndescription: proj agent\n---\nbody\n");

        let ext = discover(proj.path(), home.path());
        let names: Vec<_> = ext.agents.iter().map(|a| a.name.as_str()).collect();
        assert!(names.contains(&"u"));
        assert!(names.contains(&"p"));
    }

    #[test]
    fn project_overrides_user_by_name() {
        let home = tempdir().unwrap();
        let proj = tempdir().unwrap();
        write_agent(home.path(), "dup.md", "---\nname: dup\ndescription: USER\n---\nbody\n");
        write_agent(proj.path(), "dup.md", "---\nname: dup\ndescription: PROJECT\n---\nbody\n");

        let ext = discover(proj.path(), home.path());
        let dup: Vec<_> = ext.agents.iter().filter(|a| a.name == "dup").collect();
        assert_eq!(dup.len(), 1, "name collision should collapse to one");
        assert_eq!(dup[0].description, "PROJECT");
    }

    #[test]
    fn malformed_files_are_skipped_not_fatal() {
        let home = tempdir().unwrap();
        let proj = tempdir().unwrap();
        write_agent(proj.path(), "good.md", "---\nname: good\ndescription: d\n---\nbody\n");
        write_agent(proj.path(), "bad.md", "no frontmatter here");

        let ext = discover(proj.path(), home.path());
        let names: Vec<_> = ext.agents.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, vec!["good"]);
    }

    #[test]
    fn missing_dirs_yield_empty() {
        let home = tempdir().unwrap();
        let proj = tempdir().unwrap();
        assert_eq!(discover(proj.path(), home.path()), Extensions::default());
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p otto-extensions tests::`
Expected: FAIL — `discovers_from_both_roots`, `project_overrides_user_by_name`, `malformed_files_are_skipped_not_fatal` fail (stub returns empty).

- [ ] **Step 3: Implement `discover`**

Replace the `discover` stub in `crates/extensions/src/lib.rs`:

```rust
pub fn discover(project_root: &Path, home: &Path) -> Extensions {
    // User-global first, then project — so a later project insert overrides by name.
    let mut by_name: std::collections::BTreeMap<String, CustomAgentDef> =
        std::collections::BTreeMap::new();
    for base in [home, project_root] {
        for def in read_agents_dir(&base.join(".claude").join("agents")) {
            by_name.insert(def.name.clone(), def);
        }
    }
    Extensions {
        agents: by_name.into_values().collect(),
    }
}

/// Parse every `*.md` in `dir` (non-recursive). Missing dir → empty; unreadable/malformed
/// files are skipped with a warning, never fatal.
fn read_agents_dir(dir: &Path) -> Vec<CustomAgentDef> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return out,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("warning: skipping unreadable agent {}: {e}", path.display());
                continue;
            }
        };
        match parse_agent_md(&text) {
            Ok(def) => out.push(def),
            Err(e) => eprintln!("warning: skipping malformed agent {}: {e}", path.display()),
        }
    }
    out
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p otto-extensions`
Expected: PASS — all parser + discovery tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/extensions/src/lib.rs
git commit -m "feat(extensions): discover .claude/agents (project over user-global)"
```

---

## Task 3: `engine-core` additions — `Task` request/output + `ToolRegistry::subset`

**Files:**
- Modify: `crates/engine-core/src/types.rs`
- Modify: `crates/engine-core/src/tool.rs`

- [ ] **Step 1: Add the `Task` variants**

In `crates/engine-core/src/types.rs`, add a variant to `AgentRequest` (after `Verify,`):

```rust
    Verify,
    /// A free-form subagent task (custom agents). The fixed spine never constructs this.
    Task {
        prompt: String,
    },
}
```

And to `AgentOutput` (after `Verify { ok: bool, detail: String },`):

```rust
    Verify { ok: bool, detail: String },
    /// A free-form subagent result (custom agents).
    Task { text: String },
}
```

- [ ] **Step 2: Verify the workspace still builds**

Run: `cargo build --workspace`
Expected: PASS — agents use `let-else`/catch-all matches, so the new variants compile cleanly. If any non-exhaustive `match` on `AgentRequest`/`AgentOutput` surfaces, add a `_ => unreachable!()` arm or handle it; none are expected.

- [ ] **Step 3: Write the failing `subset` test**

In `crates/engine-core/src/tool.rs`, inside the existing `#[cfg(test)] mod tests`, add tests (the `AllowAll`, `DenyAll`, `EchoTool`, `DenyAsk`, and `registry(..)` helpers already exist in this module):

```rust
    struct PingTool;
    #[async_trait]
    impl Tool for PingTool {
        fn name(&self) -> &str {
            "ping"
        }
        async fn call(&self, _args: Value) -> anyhow::Result<Value> {
            Ok(json!("pong"))
        }
    }

    #[tokio::test]
    async fn subset_keeps_only_named_tools() {
        let mut r = ToolRegistry::new(Arc::new(AllowAll), Arc::new(DenyAsk));
        r.register(Arc::new(EchoTool));
        r.register(Arc::new(PingTool));

        let sub = r.subset(&["ping".to_string()]);
        assert!(sub.call("ping", json!({})).await.is_ok());
        // "echo" was excluded by the allowlist → no such tool in the subset.
        assert!(sub.call("echo", json!({})).await.is_err());
    }

    #[tokio::test]
    async fn subset_preserves_gate_denials() {
        let mut r = ToolRegistry::new(Arc::new(DenyAll), Arc::new(DenyAsk));
        r.register(Arc::new(PingTool));
        let sub = r.subset(&["ping".to_string()]);
        // Tool is present in the allowlist, but the shared gate still denies it.
        assert!(sub.call("ping", json!({})).await.is_err());
    }
```

- [ ] **Step 4: Run the test to verify it fails**

Run: `cargo test -p otto-engine-core tool::tests::subset`
Expected: FAIL — `no method named subset found for struct ToolRegistry`.

- [ ] **Step 5: Implement `subset`**

In `crates/engine-core/src/tool.rs`, add a method inside `impl ToolRegistry` (after `register`):

```rust
    /// A new registry holding only the named tools, sharing this registry's gate and ask
    /// resolver. An allowlist can only NARROW the available tools — the sensitive-path floor
    /// and every gate decision are identical to the parent. Names not present here are dropped.
    pub fn subset(&self, allowed: &[String]) -> ToolRegistry {
        let tools = allowed
            .iter()
            .filter_map(|name| self.tools.get(name).map(|t| (name.clone(), Arc::clone(t))))
            .collect();
        ToolRegistry {
            tools,
            gate: Arc::clone(&self.gate),
            ask: Arc::clone(&self.ask),
        }
    }
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test -p otto-engine-core tool::`
Expected: PASS — both `subset` tests pass; existing tool tests stay green.

- [ ] **Step 7: Commit**

```bash
git add crates/engine-core/src/types.rs crates/engine-core/src/tool.rs
git commit -m "feat(engine-core): AgentRequest/Output Task variants + ToolRegistry::subset"
```

---

## Task 4: `MarkdownAgent`

**Files:**
- Create: `crates/extensions/src/markdown_agent.rs`
- Modify: `crates/extensions/src/lib.rs`

- [ ] **Step 1: Declare the module and re-export**

In `crates/extensions/src/lib.rs`, add the module declaration (with the other `mod`) and the re-export:

```rust
mod agent_def;
mod markdown_agent;

pub use agent_def::{CustomAgentDef, parse_agent_md};
pub use markdown_agent::MarkdownAgent;
```

- [ ] **Step 2: Write the failing MarkdownAgent test**

Create `crates/extensions/src/markdown_agent.rs`:

```rust
//! A custom agent loaded from `agents/*.md`. It answers a free-form `AgentRequest::Task` by
//! running its markdown body as a system prompt through the router. It uses whatever tool
//! view its `AgentCtx` carries — the dispatcher (`TaskTool`) supplies the allowlist-filtered
//! subset.

use async_trait::async_trait;
use otto_engine_core::traits::{Agent, AgentCtx};
use otto_engine_core::router::RouteHints;
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
    use otto_engine_core::tool::{
        AskResolver, Decision, DenyAsk, PermissionGate, ToolRegistry,
    };
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
            Ok(CompleteResponse { text: req.prompt, usage: None })
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
        let tools = ToolRegistry::new(Arc::new(AllowAll), Arc::new(DenyAsk) as Arc<dyn AskResolver>);
        let ctx = AgentCtx::new(&router, &ws, &tools);

        let out = agent
            .run(AgentRequest::Task { prompt: "do the thing".into() }, &ctx)
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
```

- [ ] **Step 3: Run the test to verify it fails**

Run: `cargo test -p otto-extensions markdown_agent::`
Expected: FAIL — if `AgentCtx::new`/trait paths differ, the compile error names the exact path. (Cross-check imports against `crates/engine-core/src/traits.rs` and `tool.rs`; `Agent`, `AgentCtx`, `WorkspaceRead` are in `traits`, `Router`/`RouteHints` in `router`, `ToolRegistry`/`PermissionGate`/`AskResolver`/`Decision`/`DenyAsk` in `tool`.)

- [ ] **Step 4: Confirm the implementation compiles and passes**

The implementation is written in Step 2 alongside the test. If the test failed only because the module wasn't yet declared or an import path was off, fix the import path and re-run.

Run: `cargo test -p otto-extensions markdown_agent::`
Expected: PASS — both tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/extensions/src/lib.rs crates/extensions/src/markdown_agent.rs
git commit -m "feat(extensions): MarkdownAgent runs its body as a system prompt"
```

---

## Task 5: `TaskTool` (subagent dispatch)

**Files:**
- Create: `crates/extensions/src/task_tool.rs`
- Modify: `crates/extensions/src/lib.rs`

- [ ] **Step 1: Declare the module and re-export**

In `crates/extensions/src/lib.rs`:

```rust
mod agent_def;
mod markdown_agent;
mod task_tool;

pub use agent_def::{CustomAgentDef, parse_agent_md};
pub use markdown_agent::MarkdownAgent;
pub use task_tool::TaskTool;
```

- [ ] **Step 2: Write the failing TaskTool tests**

Create `crates/extensions/src/task_tool.rs`:

```rust
//! `task`: a built-in tool that dispatches a named custom agent as a depth-1 sub-turn. The
//! dispatched agent gets a tool view filtered to its `tools` allowlist (shared gate/ask, so
//! the sensitive-path floor is preserved). The base registry passed in never contains `task`,
//! so a dispatched agent cannot re-dispatch.

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
}

impl TaskTool {
    pub fn new(
        router: Arc<dyn Router>,
        workspace: Arc<dyn WorkspaceRead>,
        agents: Arc<AgentRegistry>,
        base_tools: Arc<ToolRegistry>,
    ) -> Self {
        Self { router, workspace, agents, base_tools }
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

        // Filtered tool view: the allowlist read from the registered agent. We re-read the
        // allowlist via a downcast-free convention — agents that expose no allowlist get all
        // base tools. Slice 1 agents are `MarkdownAgent`; its allowlist drives the subset.
        let sub_tools = match self.allowlist_for(name) {
            Some(allowed) => self.base_tools.subset(&allowed),
            None => self.base_tools.subset(&self.base_tools.tool_names()),
        };

        let ctx = AgentCtx::new(self.router.as_ref(), self.workspace.as_ref(), &sub_tools);
        let out = agent
            .run(AgentRequest::Task { prompt: prompt.to_string() }, &ctx)
            .await?;
        match out {
            AgentOutput::Task { text } => Ok(json!({ "text": text })),
            other => anyhow::bail!("task: agent returned non-Task output: {other:?}"),
        }
    }
}
```

This references two helpers that don't exist yet — `TaskTool::allowlist_for` and `ToolRegistry::tool_names`. They are added in Steps 4–5. First write the tests:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_def::CustomAgentDef;
    use crate::markdown_agent::MarkdownAgent;
    use otto_engine_core::router::{RouteHints, Router as _};
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
            Ok(CompleteResponse { text: req.prompt, usage: None })
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
        TaskTool::new(
            Arc::new(EchoRouter),
            Arc::new(StubWorkspace),
            Arc::new(reg),
            Arc::new(base),
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
}
```

- [ ] **Step 3: Run the tests to verify they fail (compile error)**

Run: `cargo test -p otto-extensions task_tool::`
Expected: FAIL — `no method named allowlist_for` / `no method named tool_names`.

- [ ] **Step 4: Add `ToolRegistry::tool_names` in engine-core**

In `crates/engine-core/src/tool.rs`, inside `impl ToolRegistry` (after `subset`):

```rust
    /// The names of every registered tool. Lets a dispatcher request "all tools" as a subset.
    pub fn tool_names(&self) -> Vec<String> {
        self.tools.keys().cloned().collect()
    }
```

- [ ] **Step 5: Add `TaskTool::allowlist_for`**

The dispatcher needs the named agent's allowlist. Rather than downcasting the `Arc<dyn Agent>`, keep the allowlist where dispatch can see it: hold an index on `TaskTool`. Replace the `TaskTool` struct/constructor in `crates/extensions/src/task_tool.rs` so the allowlist is captured at construction, and add the helper:

```rust
pub struct TaskTool {
    router: Arc<dyn Router>,
    workspace: Arc<dyn WorkspaceRead>,
    agents: Arc<AgentRegistry>,
    base_tools: Arc<ToolRegistry>,
    allowlists: std::collections::HashMap<String, Option<Vec<String>>>,
}

impl TaskTool {
    pub fn new(
        router: Arc<dyn Router>,
        workspace: Arc<dyn WorkspaceRead>,
        agents: Arc<AgentRegistry>,
        base_tools: Arc<ToolRegistry>,
        allowlists: std::collections::HashMap<String, Option<Vec<String>>>,
    ) -> Self {
        Self { router, workspace, agents, base_tools, allowlists }
    }

    fn allowlist_for(&self, name: &str) -> Option<Vec<String>> {
        self.allowlists.get(name).cloned().flatten()
    }
}
```

Update the `call` body's filtered-view block to use the helper (it already calls `self.allowlist_for(name)`), and update the test `fn tool()` to pass the allowlist map:

```rust
        let mut allowlists = std::collections::HashMap::new();
        allowlists.insert("echoer".to_string(), Some(vec!["fs.read".to_string()]));
        TaskTool::new(
            Arc::new(EchoRouter),
            Arc::new(StubWorkspace),
            Arc::new(reg),
            Arc::new(base),
            allowlists,
        )
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test -p otto-extensions`
Expected: PASS — all extensions tests pass (parser, discovery, MarkdownAgent, TaskTool).

- [ ] **Step 7: Add a filtered-tools end-to-end test**

This proves `TaskTool` hands the agent the allowlist-filtered tools (not just that dispatch returns text). Add a probe agent + test to the `tests` module in `crates/extensions/src/task_tool.rs`:

```rust
    struct ProbeAgent;
    #[async_trait]
    impl otto_engine_core::traits::Agent for ProbeAgent {
        async fn run(
            &self,
            req: AgentRequest,
            ctx: &AgentCtx<'_>,
        ) -> anyhow::Result<AgentOutput> {
            let AgentRequest::Task { prompt } = req else {
                anyhow::bail!("probe expects Task");
            };
            // `prompt` names a tool to attempt; report whether it was reachable.
            let reachable = ctx.tools().call(&prompt, json!({})).await.is_ok();
            Ok(AgentOutput::Task { text: format!("reachable={reachable}") })
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
        let mut allowlists = std::collections::HashMap::new();
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
```

- [ ] **Step 8: Run the test to verify it passes**

Run: `cargo test -p otto-extensions task_tool::dispatched_agent_only_sees_allowlisted_tools`
Expected: PASS — the empty allowlist hides `ping`.

- [ ] **Step 9: Commit**

```bash
git add crates/extensions/src/lib.rs crates/extensions/src/task_tool.rs crates/engine-core/src/tool.rs
git commit -m "feat(extensions): TaskTool dispatches custom agents with filtered tools"
```

---

## Task 6: Wire `otto run --agent <name> "<goal>"`

**Files:**
- Modify: `crates/engine/Cargo.toml`
- Modify: `crates/engine/src/main.rs`

- [ ] **Step 1: Add the dependency**

In `crates/engine/Cargo.toml`, under `[dependencies]`, add:

```toml
otto-extensions = { path = "../extensions" }
```

- [ ] **Step 2: Add an `--agent` parser helper + a failing integration test**

In `crates/engine/src/main.rs`, add a helper next to `parse_root` (around line 34):

```rust
/// Parse `--agent <name>` from args. Returns (Some(name), remaining) or (None, args).
fn parse_agent_flag(args: &[String]) -> (Option<String>, Vec<String>) {
    let mut name = None;
    let mut rest = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--agent" {
            match it.next() {
                Some(v) => name = Some(v.clone()),
                None => {
                    eprintln!("error: --agent requires a name");
                    std::process::exit(2);
                }
            }
        } else {
            rest.push(a.clone());
        }
    }
    (name, rest)
}

/// The user-global `.claude/` base: `$HOME` (empty path if unset → discovery yields nothing).
fn home_dir() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default())
}
```

Add an integration test at the bottom of `crates/engine/src/main.rs` (create a `#[cfg(test)] mod tests` if none exists):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_agent_flag_extracts_name() {
        let args = vec![
            "--agent".to_string(),
            "reviewer".to_string(),
            "do it".to_string(),
        ];
        let (name, rest) = parse_agent_flag(&args);
        assert_eq!(name, Some("reviewer".to_string()));
        assert_eq!(rest, vec!["do it".to_string()]);
    }

    #[test]
    fn parse_agent_flag_absent_is_none() {
        let args = vec!["do it".to_string()];
        let (name, rest) = parse_agent_flag(&args);
        assert_eq!(name, None);
        assert_eq!(rest, vec!["do it".to_string()]);
    }
}
```

- [ ] **Step 3: Run the test to verify it fails**

Run: `cargo test -p otto-engine --bin otto parse_agent_flag`
Expected: FAIL — `parse_agent_flag` not found.

(The helper is written in Step 2; this confirms the test wiring before adding the dispatch path.)

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p otto-engine --bin otto parse_agent_flag`
Expected: PASS — both parser tests pass.

- [ ] **Step 5: Add the `--agent` dispatch path to `cmd_run`**

In `crates/engine/src/main.rs`, modify `cmd_run` to branch on `--agent`. Replace the start of `cmd_run` (the `parse_root` + goal-extraction lines) with:

```rust
async fn cmd_run(args: Vec<String>) -> anyhow::Result<()> {
    let (root, after_root) = parse_root(&args);
    let (agent_name, positional) = parse_agent_flag(&after_root);
    let goal = positional.into_iter().next().unwrap_or_else(|| {
        eprintln!("error: missing goal");
        std::process::exit(2);
    });

    if let Some(name) = agent_name {
        return run_custom_agent(&name, &goal, root).await;
    }

    // ... existing spine path unchanged below ...
```

Keep the rest of the existing `cmd_run` body as-is (the router/workspace/tools/store/retriever/`run_goal` path).

Then add the new dispatch function after `cmd_run`:

```rust
/// Run a discovered custom agent through the `TaskTool` dispatch path and print its output.
/// No-op-friendly: an unknown agent name (or no `.claude/agents/`) is a clear error.
async fn run_custom_agent(name: &str, goal: &str, root: PathBuf) -> anyhow::Result<()> {
    use otto_engine_core::AgentRegistry;
    use otto_engine_core::tool::Tool;
    use otto_extensions::{MarkdownAgent, TaskTool};
    use otto_protocol::Role;
    use std::collections::HashMap;

    let ext = otto_extensions::discover(&root, &home_dir());

    let mut registry = AgentRegistry::new();
    let mut allowlists: HashMap<String, Option<Vec<String>>> = HashMap::new();
    for def in ext.agents {
        allowlists.insert(def.name.clone(), def.tools.clone());
        registry.register(
            Role::Custom(def.name.clone()),
            Arc::new(MarkdownAgent::new(def)),
        );
    }

    if registry.get(&Role::Custom(name.to_string())).is_err() {
        anyhow::bail!("no custom agent named '{name}' under .claude/agents/");
    }

    let router: Arc<dyn otto_engine_core::Router> = Arc::from(build_router());
    let read_ws: Arc<dyn otto_engine_core::WorkspaceRead> =
        Arc::new(LocalWorkspace::new(root.clone()));
    let tools_ws: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(root.clone()));
    let (base_tools, _mcp) = build_tools_preferring_mcp(tools_ws, root, false).await;

    let task = TaskTool::new(
        router,
        read_ws,
        Arc::new(registry),
        Arc::new(base_tools),
        allowlists,
    );
    let out = task
        .call(serde_json::json!({ "agent": name, "prompt": goal }))
        .await?;
    println!("{}", out["text"].as_str().unwrap_or_default());
    Ok(())
}
```

Note: confirm `LocalWorkspace` implements `WorkspaceRead` (it implements `Workspace: WorkspaceRead`, so the `Arc<dyn WorkspaceRead>` coercion is valid). If the existing imports don't already bring `WorkspaceRead`/`Workspace` into scope in `main.rs`, add `use otto_engine_core::{Workspace, WorkspaceRead};` (cross-check the existing `use` lines — `Workspace` is already imported).

- [ ] **Step 6: Build and run the full workspace test suite**

Run: `cargo build --workspace`
Expected: PASS.

Run: `cargo test --workspace`
Expected: PASS — all existing tests stay green (the spine path is unchanged; discovery is only reached via `--agent`).

- [ ] **Step 7: Manual end-to-end smoke test**

```bash
mkdir -p /tmp/otto-ext-demo/.claude/agents
cat > /tmp/otto-ext-demo/.claude/agents/echoer.md <<'EOF'
---
name: echoer
description: echoes the task back
tools: fs.read
---
You are an echo agent. Repeat the user's request.
EOF
cargo run -p otto-engine -- run --root /tmp/otto-ext-demo --agent echoer "hello world"
```

Expected: prints the offline `LocalProvider` completion of the composed prompt (the deterministic text includes the system prompt + "hello world"). An unknown `--agent nope` exits non-zero with "no custom agent named 'nope'".

- [ ] **Step 8: Update the usage string**

In `crates/engine/src/main.rs`, update the `run` usage line (the `eprintln!` in the help/`_` arm and the top-of-file doc comment) to include the new flag:

```rust
//! `otto run "<goal>" [--root <path>] [--agent <name>]` — run a single turn (or a named custom agent) and print output.
```

and the runtime usage string:

```rust
"usage:\n  otto run \"<goal>\" [--root <path>] [--agent <name>]\n  otto serve [--root <path>] [--port <p>] [--approve-edits] [--promote-loopback | --promote-vps <ws-endpoint> | --promote-microvm] [--accept-promotions]"
```

- [ ] **Step 9: Commit**

```bash
git add crates/engine/Cargo.toml crates/engine/src/main.rs
git commit -m "feat(engine): otto run --agent dispatches a discovered custom agent"
```

---

## Task 7: Documentation

**Files:**
- Modify: `CLAUDE.md`
- Modify: `docs/ARCHITECTURE.md`

- [ ] **Step 1: Add the `extensions` crate to the CLAUDE.md crate table**

In `CLAUDE.md`, add a row to the crate table (after the `retrieval` row):

```markdown
| `extensions` | Loads otto's native extension format (Claude Code's `.claude/`). Slice 1: discovers `agents/*.md` from `~/.claude/` + project `.claude/` (project wins), parses Claude-Code-exact frontmatter into `CustomAgentDef`, exposes a `MarkdownAgent` (`Agent` running its body as a system prompt) and a `TaskTool` (`Tool` dispatching a named custom agent as a depth-1, allowlist-filtered sub-turn). Wired into `otto run --agent <name>`. Depends inward on `engine-core`/`protocol`; invoked only by the binary (the offline suite is untouched). |
```

- [ ] **Step 2: Note the slice in the architecture doc**

In `docs/ARCHITECTURE.md`, update the `extensions` line in the crate tree (around line 38) to reflect slice 1 shipped:

```
│   ├── extensions       # Loads .claude/ agents, commands, skills, hooks, permissions, plugins.
│   │                    #   Slice 1 shipped: custom agents (discover + MarkdownAgent + Task dispatch).
```

- [ ] **Step 3: Commit**

```bash
git add CLAUDE.md docs/ARCHITECTURE.md
git commit -m "docs(extensions): record custom-agents slice 1"
```

---

## Done criteria

- `cargo test --workspace` is green; `cargo fmt --all` and `cargo clippy --workspace --all-targets` are clean.
- `otto run --agent <name> "<goal>"` runs a discovered custom agent and prints its output; an unknown name errors.
- The offline determinism suite is unchanged (no `.claude/` ⇒ no discovery, spine path identical).
- Deferred to later slices (recorded in the spec Non-goals): autonomous spine dispatch (+ spine registration of `task`), nested dispatch, `model`-hint routing, and the other five artifact types.
