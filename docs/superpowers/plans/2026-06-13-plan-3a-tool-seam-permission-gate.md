# otto Plan 3a — Tool Seam & Permission Gate Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give otto's agents a way to call tools through a `Tool` seam, with every call passing a deterministic permission gate (otto's guardrail), and ship the first in-process tools (filesystem read/write/list over the path-contained `Workspace`).

**Architecture:** Builds additively on Plans 1–2. A new `Tool` trait in `engine-core` (MCP-shaped: name + JSON args/result) plus a `ToolRegistry` that runs a `PermissionGate` before dispatching every call. A new `otto-tools` crate provides the `DefaultPermissionGate` (sensitive-path-floor + allow/deny rules) and the in-process `fs.read`/`fs.write`/`fs.list` tools over `Arc<dyn Workspace>`. `AgentCtx` gains a `tools()` accessor so agents reach tools the same way they reach the router. Because tools are JSON-in/JSON-out behind a trait, an MCP-stdio `Tool` impl (rmcp client to external servers) slots in behind this exact seam in Plan 3b.

**Tech Stack:** Rust (edition 2024), tokio, async-trait, anyhow, serde_json, tempfile (dev).

---

## Context for the implementer (read once)

Established by Plans 1–2 (on `main`):
- `engine-core`: `Provider`/`Workspace`/`Agent`/`Router` traits; `AgentCtx<'a>` with PRIVATE fields and `AgentCtx::new(router: &dyn Router, workspace: &dyn Workspace)` + `router()`/`workspace()` accessors; `Orchestrator<'a> { registry, router, workspace }`; `run_turn` builds `AgentCtx::new(self.router, self.workspace)`.
- `Workspace` trait: `async fn read(&self, &Path) -> Result<Vec<u8>>`, `async fn list(&self, glob: &str) -> Result<Vec<PathBuf>>`, `async fn apply_edit(&self, &Edit) -> Result<u64>`. `Edit { path: PathBuf, new_contents: String }`. `LocalWorkspace` enforces path containment.
- `otto-agents`: `StubPlanner`, `StubContextFinder`, `EchoCoder` (calls `ctx.router()`), `StubVerifier`. The four impls use `ctx: &AgentCtx` (elided lifetime — NEVER `<'_>` in `impl Agent`, or E0195).
- `otto-engine`: `run_goal(goal, router: &dyn Router, workspace: &dyn Workspace)`, `build_router()`, `build_default_registry()`, `otto run` CLI.

**Conventions (carry forward):**
- Git hygiene: stay on branch `feat/plan-3-mcp-tools`. NEVER `git checkout <sha>` / detach HEAD. Only `git add` + `git commit` (no `--amend` across tasks). Commit `Cargo.lock` alongside crate changes when it updates.
- No AI/Claude self-attribution in commit messages.
- Per-package gates (`cargo test/clippy/fmt -p <crate>`), then a final workspace gate. `clippy -D warnings` must be clean — watch unused imports (scope test-only imports into the test module).
- TDD: failing test → minimal impl → green → commit.

---

## File Structure

```
crates/
├── engine-core/src/
│   ├── tool.rs          # NEW: Tool trait, Decision, PermissionGate, AskResolver, DenyAsk, ToolRegistry
│   ├── traits.rs        # MODIFY: AgentCtx gains `tools` field + tools() accessor; new() takes tools
│   ├── orchestrator.rs  # MODIFY: Orchestrator holds `tools: &ToolRegistry`; builds ctx with it
│   └── lib.rs           # MODIFY: export tool items
├── tools/               # NEW CRATE: otto-tools
│   ├── Cargo.toml
│   └── src/
│       ├── lib.rs       # module root + re-exports
│       ├── gate.rs      # DefaultPermissionGate (sensitive-path floor + rules)
│       └── fs.rs        # FsReadTool, FsWriteTool, FsListTool over Arc<dyn Workspace>
├── agents/src/lib.rs    # MODIFY: compile against new AgentCtx; ContextFinder uses fs.list
└── engine/src/
    ├── lib.rs           # MODIFY: build_tool_registry(); run_goal takes &ToolRegistry
    └── main.rs          # MODIFY: build registry, pass to run_goal
```

**Responsibility boundaries:** `engine-core` owns the `Tool`/`ToolRegistry`/gate *seam* (no concrete tools). `otto-tools` owns concrete tools + the default gate, depends on `engine-core` only. `engine` wires the registry with the session's workspace. Add `crates/tools` to the workspace `members`.

---

## Task 1: `Tool` seam + `ToolRegistry` + permission gate in engine-core

**Files:**
- Create: `crates/engine-core/src/tool.rs`
- Modify: `crates/engine-core/src/lib.rs`

- [ ] **Step 1: Write tool.rs with tests**

Create `crates/engine-core/src/tool.rs`:

```rust
//! The tool seam. Agents call tools through a `ToolRegistry` that runs a deterministic
//! `PermissionGate` (otto's guardrail) before dispatching. Tools are MCP-shaped — a name
//! plus JSON args in / JSON result out — so an MCP-stdio tool (rmcp client) can register
//! behind this same `Tool` trait later.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

/// A callable tool: a stable name and a JSON-in / JSON-out call.
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    async fn call(&self, args: Value) -> anyhow::Result<Value>;
}

/// The verdict a permission gate returns for a proposed tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Ask,
    Deny,
}

/// Deterministic, non-LLM evaluation of a proposed tool call — otto's guardrail for tools.
pub trait PermissionGate: Send + Sync {
    fn evaluate(&self, tool: &str, args: &Value) -> Decision;
}

/// Resolves an `Ask` verdict to allow (true) or deny (false). Headless mode uses `DenyAsk`
/// (safe default); the interactive UI supplies a prompting resolver in a later plan.
pub trait AskResolver: Send + Sync {
    fn resolve(&self, tool: &str, args: &Value) -> bool;
}

/// Headless default: deny anything that requires asking.
pub struct DenyAsk;

impl AskResolver for DenyAsk {
    fn resolve(&self, _tool: &str, _args: &Value) -> bool {
        false
    }
}

/// Holds the available tools plus the gate/resolver. Every `call` is gated before dispatch.
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
    gate: Arc<dyn PermissionGate>,
    ask: Arc<dyn AskResolver>,
}

impl ToolRegistry {
    pub fn new(gate: Arc<dyn PermissionGate>, ask: Arc<dyn AskResolver>) -> Self {
        Self { tools: HashMap::new(), gate, ask }
    }

    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        self.tools.insert(tool.name().to_string(), tool);
    }

    /// Gate then dispatch. Denied (or ask-denied) calls error before the tool runs.
    pub async fn call(&self, name: &str, args: Value) -> anyhow::Result<Value> {
        match self.gate.evaluate(name, &args) {
            Decision::Deny => anyhow::bail!("tool '{name}' denied by permission gate"),
            Decision::Ask => {
                if !self.ask.resolve(name, &args) {
                    anyhow::bail!("tool '{name}' not permitted (ask denied)");
                }
            }
            Decision::Allow => {}
        }
        let tool = self
            .tools
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("no tool registered named '{name}'"))?;
        tool.call(args).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct EchoTool;
    #[async_trait]
    impl Tool for EchoTool {
        fn name(&self) -> &str {
            "echo"
        }
        async fn call(&self, args: Value) -> anyhow::Result<Value> {
            Ok(json!({ "echoed": args }))
        }
    }

    struct AllowAll;
    impl PermissionGate for AllowAll {
        fn evaluate(&self, _tool: &str, _args: &Value) -> Decision {
            Decision::Allow
        }
    }

    struct DenyAll;
    impl PermissionGate for DenyAll {
        fn evaluate(&self, _tool: &str, _args: &Value) -> Decision {
            Decision::Deny
        }
    }

    struct AskGate;
    impl PermissionGate for AskGate {
        fn evaluate(&self, _tool: &str, _args: &Value) -> Decision {
            Decision::Ask
        }
    }

    fn registry(gate: Arc<dyn PermissionGate>, ask: Arc<dyn AskResolver>) -> ToolRegistry {
        let mut r = ToolRegistry::new(gate, ask);
        r.register(Arc::new(EchoTool));
        r
    }

    #[tokio::test]
    async fn allowed_call_dispatches_to_tool() {
        let r = registry(Arc::new(AllowAll), Arc::new(DenyAsk));
        let out = r.call("echo", json!({"x": 1})).await.unwrap();
        assert_eq!(out, json!({ "echoed": { "x": 1 } }));
    }

    #[tokio::test]
    async fn denied_call_never_dispatches() {
        let r = registry(Arc::new(DenyAll), Arc::new(DenyAsk));
        let err = r.call("echo", json!({})).await.unwrap_err();
        assert!(err.to_string().contains("denied by permission gate"));
    }

    #[tokio::test]
    async fn ask_resolved_to_deny_blocks_call() {
        let r = registry(Arc::new(AskGate), Arc::new(DenyAsk));
        let err = r.call("echo", json!({})).await.unwrap_err();
        assert!(err.to_string().contains("ask denied"));
    }

    #[tokio::test]
    async fn unknown_tool_errors() {
        let r = registry(Arc::new(AllowAll), Arc::new(DenyAsk));
        let err = r.call("nope", json!({})).await.unwrap_err();
        assert!(err.to_string().contains("no tool registered"));
    }
}
```

- [ ] **Step 2: Export from lib.rs**

In `crates/engine-core/src/lib.rs`, add `pub mod tool;` and re-exports. The re-export line set should include (merge with existing):

```rust
pub mod tool;
```

and add to the re-exports:

```rust
pub use tool::{AskResolver, Decision, DenyAsk, PermissionGate, Tool, ToolRegistry};
```

- [ ] **Step 3: Test**

Run: `cargo test -p otto-engine-core tool::` → 4 tests pass. Then `cargo test -p otto-engine-core` (all engine-core tests still pass), `cargo clippy -p otto-engine-core --all-targets -- -D warnings` (clean), `cargo fmt -p otto-engine-core` (clean).

- [ ] **Step 4: Commit**

```bash
git add crates/engine-core/src/tool.rs crates/engine-core/src/lib.rs
git commit -m "feat(engine-core): Tool seam + ToolRegistry with deterministic permission gate"
```

---

## Task 2: `otto-tools` crate + `DefaultPermissionGate`

**Files:**
- Modify: `Cargo.toml` (workspace members)
- Create: `crates/tools/Cargo.toml`
- Create: `crates/tools/src/lib.rs`
- Create: `crates/tools/src/gate.rs`

- [ ] **Step 1: Add the crate to the workspace**

In root `Cargo.toml`, add `"crates/tools"` to `members` after `"crates/router"`:

```toml
members = [
    "crates/protocol",
    "crates/engine-core",
    "crates/workspace",
    "crates/providers",
    "crates/router",
    "crates/tools",
    "crates/agents",
    "crates/engine",
]
```

- [ ] **Step 2: Crate manifest**

Create `crates/tools/Cargo.toml`:

```toml
[package]
name = "otto-tools"
version = "0.1.0"
edition.workspace = true
rust-version.workspace = true
license.workspace = true

[dependencies]
otto-engine-core = { path = "../engine-core" }
async-trait.workspace = true
anyhow.workspace = true
serde_json.workspace = true

[dev-dependencies]
otto-workspace = { path = "../workspace" }
tokio = { workspace = true, features = ["macros", "rt-multi-thread", "fs"] }
tempfile.workspace = true
```

- [ ] **Step 3: Write `gate.rs` with tests**

Create `crates/tools/src/gate.rs`. The default gate denies any call whose JSON args contain a `path` (or `paths`) matching a sensitive pattern (`.env`, `.ssh/`, `.git/`), and allows everything else. This is the inviolable sensitive-path floor; richer allow/ask rules arrive with the interactive UI.

```rust
//! `DefaultPermissionGate`: otto's built-in guardrail. Denies tool calls that touch
//! sensitive paths (the inviolable floor); allows everything else for now.

use otto_engine_core::tool::{Decision, PermissionGate};
use serde_json::Value;

/// Substrings that mark a path as sensitive. A tool-call argument naming such a path is denied.
const SENSITIVE_MARKERS: &[&str] = &[".env", ".ssh/", ".ssh", ".git/", "id_rsa", ".aws/"];

pub struct DefaultPermissionGate;

impl DefaultPermissionGate {
    pub fn new() -> Self {
        Self
    }

    /// True if `s` names a sensitive path.
    fn is_sensitive(s: &str) -> bool {
        SENSITIVE_MARKERS.iter().any(|m| s.contains(m))
    }

    /// Collect candidate path strings from common arg shapes: `path`, `paths[]`.
    fn candidate_paths(args: &Value) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(p) = args.get("path").and_then(Value::as_str) {
            out.push(p.to_string());
        }
        if let Some(arr) = args.get("paths").and_then(Value::as_array) {
            for v in arr {
                if let Some(p) = v.as_str() {
                    out.push(p.to_string());
                }
            }
        }
        out
    }
}

impl Default for DefaultPermissionGate {
    fn default() -> Self {
        Self::new()
    }
}

impl PermissionGate for DefaultPermissionGate {
    fn evaluate(&self, _tool: &str, args: &Value) -> Decision {
        for p in Self::candidate_paths(args) {
            if Self::is_sensitive(&p) {
                return Decision::Deny;
            }
        }
        Decision::Allow
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn denies_dotenv_path() {
        let gate = DefaultPermissionGate::new();
        assert_eq!(gate.evaluate("fs.read", &json!({"path": ".env"})), Decision::Deny);
        assert_eq!(
            gate.evaluate("fs.read", &json!({"path": "config/.env.local"})),
            Decision::Deny
        );
    }

    #[test]
    fn denies_ssh_and_git_internal() {
        let gate = DefaultPermissionGate::new();
        assert_eq!(gate.evaluate("fs.read", &json!({"path": ".ssh/id_rsa"})), Decision::Deny);
        assert_eq!(gate.evaluate("fs.write", &json!({"path": ".git/config"})), Decision::Deny);
    }

    #[test]
    fn allows_ordinary_paths() {
        let gate = DefaultPermissionGate::new();
        assert_eq!(gate.evaluate("fs.read", &json!({"path": "src/main.rs"})), Decision::Allow);
        assert_eq!(gate.evaluate("fs.list", &json!({})), Decision::Allow);
    }

    #[test]
    fn denies_when_any_path_in_list_is_sensitive() {
        let gate = DefaultPermissionGate::new();
        let args = json!({"paths": ["src/a.rs", ".ssh/known_hosts"]});
        assert_eq!(gate.evaluate("fs.search", &args), Decision::Deny);
    }
}
```

- [ ] **Step 4: lib.rs module root (gate only for now)**

Create `crates/tools/src/lib.rs`:

```rust
//! otto in-process tools (behind `otto_engine_core::Tool`) and the default permission gate.

pub mod gate;

pub use gate::DefaultPermissionGate;
```

- [ ] **Step 5: Test**

Run: `cargo test -p otto-tools` → 4 gate tests pass. `cargo clippy -p otto-tools --all-targets -- -D warnings` (clean), `cargo fmt -p otto-tools` (clean), then `cargo build --workspace` (new member resolves).

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml crates/tools
git commit -m "feat(tools): otto-tools crate with DefaultPermissionGate (sensitive-path floor)"
```

---

## Task 3: Filesystem tools (`fs.read`, `fs.write`, `fs.list`)

**Files:**
- Create: `crates/tools/src/fs.rs`
- Modify: `crates/tools/src/lib.rs`
- Modify: `crates/tools/Cargo.toml`

- [ ] **Step 1: Make otto-workspace a normal dependency**

The fs tools operate over a `Workspace`. They use `Arc<dyn Workspace>` (the trait from engine-core), so the *trait* is available via engine-core. But to construct tools in tests we use `LocalWorkspace`. Move `otto-workspace` from `[dev-dependencies]` is NOT required for the lib (it only needs the trait). Keep `otto-workspace` as a dev-dependency (already added in Task 2) for the tests. No manifest change needed in this step — proceed.

- [ ] **Step 2: Write `fs.rs` with tests**

Create `crates/tools/src/fs.rs`:

```rust
//! In-process filesystem tools over a path-contained `Workspace`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use otto_engine_core::tool::Tool;
use otto_engine_core::traits::Workspace;
use otto_engine_core::types::Edit;
use serde_json::{json, Value};

/// `fs.read` — args `{ "path": "<rel>" }` → `{ "content": "<utf8>" }`.
pub struct FsReadTool {
    workspace: Arc<dyn Workspace>,
}

impl FsReadTool {
    pub fn new(workspace: Arc<dyn Workspace>) -> Self {
        Self { workspace }
    }
}

#[async_trait]
impl Tool for FsReadTool {
    fn name(&self) -> &str {
        "fs.read"
    }

    async fn call(&self, args: Value) -> anyhow::Result<Value> {
        let path = args
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("fs.read requires a string 'path' arg"))?;
        let bytes = self.workspace.read(Path::new(path)).await?;
        let content = String::from_utf8(bytes)?;
        Ok(json!({ "content": content }))
    }
}

/// `fs.write` — args `{ "path": "<rel>", "contents": "<utf8>" }` → `{ "bytes_written": <n> }`.
pub struct FsWriteTool {
    workspace: Arc<dyn Workspace>,
}

impl FsWriteTool {
    pub fn new(workspace: Arc<dyn Workspace>) -> Self {
        Self { workspace }
    }
}

#[async_trait]
impl Tool for FsWriteTool {
    fn name(&self) -> &str {
        "fs.write"
    }

    async fn call(&self, args: Value) -> anyhow::Result<Value> {
        let path = args
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("fs.write requires a string 'path' arg"))?;
        let contents = args
            .get("contents")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("fs.write requires a string 'contents' arg"))?;
        let edit = Edit { path: PathBuf::from(path), new_contents: contents.to_string() };
        let bytes_written = self.workspace.apply_edit(&edit).await?;
        Ok(json!({ "bytes_written": bytes_written }))
    }
}

/// `fs.list` — args `{ "glob": "<pat>" }` (optional, defaults "*") → `{ "paths": ["<rel>", ...] }`.
pub struct FsListTool {
    workspace: Arc<dyn Workspace>,
}

impl FsListTool {
    pub fn new(workspace: Arc<dyn Workspace>) -> Self {
        Self { workspace }
    }
}

#[async_trait]
impl Tool for FsListTool {
    fn name(&self) -> &str {
        "fs.list"
    }

    async fn call(&self, args: Value) -> anyhow::Result<Value> {
        let glob = args.get("glob").and_then(Value::as_str).unwrap_or("*");
        let paths = self.workspace.list(glob).await?;
        let paths: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
        Ok(json!({ "paths": paths }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use otto_workspace::LocalWorkspace;

    fn ws() -> (tempfile::TempDir, Arc<dyn Workspace>) {
        let dir = tempfile::tempdir().unwrap();
        let ws: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(dir.path()));
        (dir, ws)
    }

    #[tokio::test]
    async fn write_then_read_round_trips() {
        let (_dir, ws) = ws();
        let write = FsWriteTool::new(Arc::clone(&ws));
        let out = write
            .call(json!({"path": "a.txt", "contents": "hello"}))
            .await
            .unwrap();
        assert_eq!(out, json!({ "bytes_written": 5 }));

        let read = FsReadTool::new(Arc::clone(&ws));
        let out = read.call(json!({"path": "a.txt"})).await.unwrap();
        assert_eq!(out, json!({ "content": "hello" }));
    }

    #[tokio::test]
    async fn list_returns_written_files() {
        let (_dir, ws) = ws();
        FsWriteTool::new(Arc::clone(&ws))
            .call(json!({"path": "a.txt", "contents": "x"}))
            .await
            .unwrap();
        let out = FsListTool::new(Arc::clone(&ws)).call(json!({})).await.unwrap();
        assert_eq!(out, json!({ "paths": ["a.txt"] }));
    }

    #[tokio::test]
    async fn read_missing_path_arg_errors() {
        let (_dir, ws) = ws();
        let err = FsReadTool::new(ws).call(json!({})).await.unwrap_err();
        assert!(err.to_string().contains("requires a string 'path'"));
    }

    #[tokio::test]
    async fn write_rejects_escape_via_workspace_containment() {
        let (_dir, ws) = ws();
        let err = FsWriteTool::new(ws)
            .call(json!({"path": "../escape.txt", "contents": "x"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("escapes workspace root"));
    }
}
```

- [ ] **Step 3: Re-export the tools**

Update `crates/tools/src/lib.rs`:

```rust
//! otto in-process tools (behind `otto_engine_core::Tool`) and the default permission gate.

pub mod fs;
pub mod gate;

pub use fs::{FsListTool, FsReadTool, FsWriteTool};
pub use gate::DefaultPermissionGate;
```

- [ ] **Step 4: Test**

Run: `cargo test -p otto-tools` → 8 tests pass (4 gate + 4 fs). `cargo clippy -p otto-tools --all-targets -- -D warnings` (clean), `cargo fmt -p otto-tools` (clean).

- [ ] **Step 5: Commit**

```bash
git add crates/tools
git commit -m "feat(tools): in-process fs.read / fs.write / fs.list over the contained Workspace"
```

---

## Task 4: Reshape `AgentCtx` + `Orchestrator` to carry the `ToolRegistry`

**Files:**
- Modify: `crates/engine-core/src/traits.rs`
- Modify: `crates/engine-core/src/orchestrator.rs`

This adds a third capability to `AgentCtx`. Thanks to the constructor pattern, the only call sites that change are `AgentCtx::new` callers (orchestrator + tests). EXPECTED partial breakage: `otto-agents` and `otto-engine` won't compile until Tasks 5–6 (only build/test `-p otto-engine-core` here).

- [ ] **Step 1: Add `tools` to `AgentCtx`**

In `crates/engine-core/src/traits.rs`, add the import and extend `AgentCtx`. Change the router import line to also bring in `ToolRegistry`:

```rust
use crate::router::Router;
use crate::tool::ToolRegistry;
use crate::types::{AgentOutput, AgentRequest, CompleteRequest, CompleteResponse, Edit};
```

Replace the `AgentCtx` struct + impl with:

```rust
/// Scoped resources an agent may use during a turn. Fields are private; construct via
/// `new` and read via accessors so capabilities can be added without breaking callers.
pub struct AgentCtx<'a> {
    router: &'a dyn Router,
    workspace: &'a dyn Workspace,
    tools: &'a ToolRegistry,
}

impl<'a> AgentCtx<'a> {
    pub fn new(
        router: &'a dyn Router,
        workspace: &'a dyn Workspace,
        tools: &'a ToolRegistry,
    ) -> Self {
        Self { router, workspace, tools }
    }

    /// The router agents call to run completions (local-vs-remote selection happens inside).
    pub fn router(&self) -> &dyn Router {
        self.router
    }

    /// The workspace agents read from / write edits to.
    pub fn workspace(&self) -> &dyn Workspace {
        self.workspace
    }

    /// The tool registry; every call is gated by the permission gate before dispatch.
    pub fn tools(&self) -> &ToolRegistry {
        self.tools
    }
}
```

- [ ] **Step 2: Orchestrator holds a `ToolRegistry`**

In `crates/engine-core/src/orchestrator.rs`, add `ToolRegistry` to the imports:

```rust
use crate::tool::ToolRegistry;
```

Add the field to the struct:

```rust
pub struct Orchestrator<'a> {
    pub registry: &'a AgentRegistry,
    pub router: &'a dyn Router,
    pub workspace: &'a dyn Workspace,
    pub tools: &'a ToolRegistry,
}
```

Update the ctx construction in `run_turn`:

```rust
        let ctx = AgentCtx::new(self.router, self.workspace, self.tools);
```

- [ ] **Step 3: Update the orchestrator inline tests**

The tests build an `Orchestrator`. They now need a `ToolRegistry`. In the `#[cfg(test)] mod tests`, add imports and a helper. Add to the test-module imports:

```rust
    use crate::tool::{AskResolver, Decision, PermissionGate, ToolRegistry};
    use serde_json::Value;
    use std::sync::Arc;
```

Add minimal test gate/resolver and a helper that builds an empty registry:

```rust
    struct TestAllowGate;
    impl PermissionGate for TestAllowGate {
        fn evaluate(&self, _tool: &str, _args: &Value) -> Decision {
            Decision::Allow
        }
    }
    struct TestDenyAsk;
    impl AskResolver for TestDenyAsk {
        fn resolve(&self, _tool: &str, _args: &Value) -> bool {
            false
        }
    }
    fn empty_tools() -> ToolRegistry {
        ToolRegistry::new(Arc::new(TestAllowGate), Arc::new(TestDenyAsk))
    }
```

In BOTH tests, construct the tools and pass them into the orchestrator. For `run_turn_drives_full_spine_and_emits_ordered_events` and `run_turn_errors_when_a_role_is_missing`, change the orchestrator construction to include `tools: &tools`:

```rust
        let reg = registry();
        let router = FakeRouter;
        let workspace = RecordingWorkspace::default();
        let tools = empty_tools();
        let orch = Orchestrator {
            registry: &reg,
            router: &router,
            workspace: &workspace,
            tools: &tools,
        };
```

(`Arc` may already be imported in the test module — if a duplicate-import error occurs, use the existing one. The fake agents are unchanged; they don't call tools.)

- [ ] **Step 4: Build & test engine-core only**

Run: `cargo test -p otto-engine-core` (all pass: tool tests + orchestrator tests + router tests), `cargo clippy -p otto-engine-core --all-targets -- -D warnings` (clean), `cargo fmt -p otto-engine-core` (clean). Do NOT run `--workspace` (agents/engine intentionally broken until Tasks 5–6).

- [ ] **Step 5: Commit**

```bash
git add crates/engine-core
git commit -m "refactor(engine-core): AgentCtx + Orchestrator carry the ToolRegistry"
```

---

## Task 5: Update agents; make `ContextFinder` use `fs.list`

**Files:**
- Modify: `crates/agents/Cargo.toml`
- Modify: `crates/agents/src/lib.rs`

This makes `otto-agents` compile against the new `AgentCtx`, and turns `StubContextFinder` into a real (minimal) `ContextFinder` that lists workspace files via the tool seam — proving agent → tools → gate → workspace end-to-end.

- [ ] **Step 1: dev-dependency on otto-tools**

In `crates/agents/Cargo.toml` `[dev-dependencies]` (keep existing `otto-providers`, `otto-workspace`, `otto-router`, `tokio`, `tempfile`), add:

```toml
otto-tools = { path = "../tools" }
serde_json.workspace = true
```

- [ ] **Step 2: Rewrite `StubContextFinder` as a tool-using `ContextFinder`**

In `crates/agents/src/lib.rs`, update imports to add `serde_json`:

```rust
use serde_json::Value;
```

Replace the `StubContextFinder` struct + impl with a `ContextFinder` that calls `fs.list` (keep the type name `StubContextFinder` so the engine's `build_default_registry` does not change — only its body changes):

```rust
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
```

(`std::path::PathBuf` is referenced fully-qualified here; `PathBuf` is also already imported at the top of the file from Plan 1 — either form is fine, keep it compiling. The other three agents are unchanged.)

- [ ] **Step 3: Update the agents test `ctx` helper for the new `AgentCtx::new`**

The `ctx` helper now needs a `ToolRegistry`. Update the test module. Change the test imports to add tools:

```rust
    use super::*;
    use otto_engine_core::tool::ToolRegistry;
    use otto_providers::LocalProvider;
    use otto_router::SingleProviderRouter;
    use otto_tools::{DefaultPermissionGate, FsListTool};
    use otto_workspace::LocalWorkspace;
    use otto_engine_core::tool::DenyAsk;
    use std::sync::Arc;
```

Replace the `ctx` helper to take and pass a tool registry:

```rust
    fn ctx<'a>(
        router: &'a SingleProviderRouter,
        workspace: &'a LocalWorkspace,
        tools: &'a ToolRegistry,
    ) -> AgentCtx<'a> {
        AgentCtx::new(router, workspace, tools)
    }
```

Both existing tests construct a router + workspace and call `ctx(&router, &ws)`. They now also need a registry. Update each test to build one. For `planner_produces_one_milestone_from_goal` and `coder_turns_completion_into_an_edit`, after building `ws`, add an empty registry (these agents don't use tools):

```rust
        let tools = ToolRegistry::new(Arc::new(DefaultPermissionGate::new()), Arc::new(DenyAsk));
```

and change the `ctx(&router, &ws)` calls to `ctx(&router, &ws, &tools)`.

- [ ] **Step 4: Add a test proving ContextFinder reaches the workspace via tools**

Add a new test to the agents test module:

```rust
    #[tokio::test]
    async fn context_finder_lists_workspace_files_through_tools() {
        let router = SingleProviderRouter::new(Arc::new(LocalProvider::new()));
        let dir = tempfile::tempdir().unwrap();
        let ws_concrete = LocalWorkspace::new(dir.path());
        // Seed a file via a write through the workspace trait used by the tool.
        use otto_engine_core::traits::Workspace;
        use otto_engine_core::types::Edit;
        ws_concrete
            .apply_edit(&Edit { path: std::path::PathBuf::from("seed.txt"), new_contents: "x".into() })
            .await
            .unwrap();

        // Build a registry with fs.list over the SAME workspace path.
        let ws_for_tool: Arc<dyn otto_engine_core::traits::Workspace> =
            Arc::new(LocalWorkspace::new(dir.path()));
        let mut registry = ToolRegistry::new(Arc::new(DefaultPermissionGate::new()), Arc::new(DenyAsk));
        registry.register(Arc::new(FsListTool::new(ws_for_tool)));

        let out = StubContextFinder
            .run(AgentRequest::FindContext { goal: "g".into() }, &ctx(&router, &ws_concrete, &registry))
            .await
            .unwrap();
        match out {
            AgentOutput::Context { files } => {
                assert!(files.contains(&std::path::PathBuf::from("seed.txt")));
            }
            other => panic!("expected Context, got {other:?}"),
        }
    }
```

- [ ] **Step 5: Test**

Run: `cargo test -p otto-agents` (3 tests: the 2 existing + the new `context_finder_lists_workspace_files_through_tools`), `cargo clippy -p otto-agents --all-targets -- -D warnings` (clean), `cargo fmt -p otto-agents` (clean). (Do NOT run `--workspace` yet — engine is still broken until Task 6.)

- [ ] **Step 6: Commit**

```bash
git add crates/agents
git commit -m "feat(agents): ContextFinder lists workspace files via the fs.list tool"
```

---

## Task 6: Engine wiring — build the `ToolRegistry`, thread it through

**Files:**
- Modify: `crates/engine/Cargo.toml`
- Modify: `crates/engine/src/lib.rs`
- Modify: `crates/engine/src/main.rs`
- Modify: `crates/engine/tests/turn.rs`

- [ ] **Step 1: Dependency on otto-tools**

In `crates/engine/Cargo.toml` `[dependencies]`, add (keep existing):

```toml
otto-tools = { path = "../tools" }
```

- [ ] **Step 2: `build_tool_registry` + thread through `run_goal`**

In `crates/engine/src/lib.rs`, add imports (merge with existing — `Arc` is already imported via `std::sync::{Arc, Mutex}`):

```rust
use otto_engine_core::tool::{DenyAsk, ToolRegistry};
use otto_engine_core::traits::Workspace as WorkspaceTrait;
use otto_tools::{DefaultPermissionGate, FsListTool, FsReadTool, FsWriteTool};
```

Add a builder that registers the fs tools over a shared workspace handle:

```rust
/// Build the default tool registry: the sensitive-path-floor gate, a deny-by-default `Ask`
/// resolver (headless), and the in-process fs tools bound to `workspace`.
pub fn build_tool_registry(workspace: Arc<dyn WorkspaceTrait>) -> ToolRegistry {
    let mut registry = ToolRegistry::new(Arc::new(DefaultPermissionGate::new()), Arc::new(DenyAsk));
    registry.register(Arc::new(FsReadTool::new(Arc::clone(&workspace))));
    registry.register(Arc::new(FsWriteTool::new(Arc::clone(&workspace))));
    registry.register(Arc::new(FsListTool::new(workspace)));
    registry
}
```

Change `run_goal` to accept the tool registry and pass it to the orchestrator. Update the signature and the `Orchestrator { ... }` construction:

```rust
pub async fn run_goal(
    goal: &str,
    router: &dyn Router,
    workspace: &dyn Workspace,
    tools: &ToolRegistry,
) -> anyhow::Result<(Vec<Event>, TurnOutcome)> {
    let registry = build_default_registry();
    let session = SessionId::new();

    let collected: Arc<Mutex<Vec<Event>>> = Arc::new(Mutex::new(Vec::new()));
    let next_seq = Arc::new(Mutex::new(0u64));
    let sink = {
        let collected = Arc::clone(&collected);
        let next_seq = Arc::clone(&next_seq);
        move |kind: EventKind| {
            let mut seq = next_seq.lock().unwrap();
            collected.lock().unwrap().push(Event { seq: *seq, session, kind });
            *seq += 1;
        }
    };

    let orchestrator = Orchestrator { registry: &registry, router, workspace, tools };
    let outcome = orchestrator.run_turn(session, goal, &sink).await?;

    let events = collected.lock().unwrap().clone();
    Ok((events, outcome))
}
```

Note: `Workspace` (the engine's existing import `otto_engine_core::traits::Workspace`) and the alias `WorkspaceTrait` are the same trait — if importing it twice causes a name clash or unused-import warning, use ONE import name consistently (`Workspace`) for both the `run_goal` param and the `build_tool_registry` `Arc<dyn Workspace>` bound, and drop the `WorkspaceTrait` alias. Make it compile clean under `-D warnings`.

- [ ] **Step 3: CLI builds the registry from the workspace**

In `crates/engine/src/main.rs`, the workspace is currently a `LocalWorkspace`. The tools need an `Arc<dyn Workspace>` over the SAME root. Build both from the same root. Update imports:

```rust
use std::sync::Arc;
use otto_engine_core::traits::Workspace;
use otto_engine::{build_router, build_tool_registry, run_goal};
use otto_workspace::LocalWorkspace;
```

Replace the workspace/router/run_goal section with:

```rust
    let router = build_router();
    let workspace = LocalWorkspace::new(&root);
    let tools_workspace: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(&root));
    let tools = build_tool_registry(tools_workspace);

    let (events, outcome) = run_goal(&goal, router.as_ref(), &workspace, &tools).await?;
```

(`root: PathBuf` — `LocalWorkspace::new` takes `impl Into<PathBuf>`; passing `&root` requires `LocalWorkspace::new` to accept `&PathBuf`. It accepts `impl Into<PathBuf>`, and `&PathBuf` implements `Into<PathBuf>` via clone? No — `&PathBuf` does NOT impl `Into<PathBuf>`. So pass `root.clone()` for the first and `root` for the second, OR clone for both. Use: `LocalWorkspace::new(root.clone())` and `Arc::new(LocalWorkspace::new(root))`. Adjust so it compiles — `root` is consumed by the last use.)

Corrected section:

```rust
    let router = build_router();
    let workspace = LocalWorkspace::new(root.clone());
    let tools_workspace: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(root));
    let tools = build_tool_registry(tools_workspace);

    let (events, outcome) = run_goal(&goal, router.as_ref(), &workspace, &tools).await?;
```

- [ ] **Step 4: Update the integration test**

In `crates/engine/tests/turn.rs`, build a registry and pass it. Update imports to add tools + Arc (Arc may already be imported):

```rust
use otto_engine::{build_tool_registry, run_goal};
use otto_engine_core::traits::Workspace;
```

After building the `LocalWorkspace`, build a registry over the same tempdir and pass it to `run_goal`. Replace the `run_goal(...)` call:

```rust
    let tools_workspace: std::sync::Arc<dyn Workspace> =
        std::sync::Arc::new(LocalWorkspace::new(dir.path()));
    let tools = build_tool_registry(tools_workspace);

    let (events, outcome) = run_goal("add a greeting", &router, &workspace, &tools)
        .await
        .unwrap();
```

All existing assertions stay (monotonic seq, file written containing "add a greeting", FileEdit present, last event TurnComplete{ok:true}). The turn now also lists files via the ContextFinder tool, but that adds no events and EchoCoder ignores the context, so the assertions are unchanged.

- [ ] **Step 5: Full workspace test + CLI smoke**

Run: `cargo test --workspace` (all pass). `cargo clippy --workspace --all-targets -- -D warnings` (clean), `cargo fmt --all -- --check` (clean). Smoke: `mkdir -p /tmp/otto-p3 && cargo run -p otto-engine -- run "add a greeting" --root /tmp/otto-p3 && cat /tmp/otto-p3/otto_output.txt` — 12-event stream, `turn ok = true`, file contains "add a greeting".

- [ ] **Step 6: Commit**

```bash
git add crates/engine
git commit -m "feat(engine): build_tool_registry + thread the ToolRegistry through run_goal"
```

---

## Task 7: Workspace quality gate + docs

**Files:**
- Modify: `docs/ARCHITECTURE.md`

- [ ] **Step 1: Document the tool seam**

In `docs/ARCHITECTURE.md`, add a `tools` crate line to the crate-layout tree (after the `router` line):

```
│   ├── tools            # In-process Tool impls (fs.read/write/list) + DefaultPermissionGate (sensitive-path floor).
```

And in "Key trait interfaces", after the `Router` subsection, add:

```markdown
### `Tool` — the tool-call seam (+ the permission gate)

Agents call tools via `AgentCtx::tools()` → `ToolRegistry::call(name, json_args)`. Tools are
MCP-shaped (`fn name()`, `async fn call(Value) -> Result<Value>`), so an MCP-stdio tool
(rmcp client to external/Claude Code servers) registers behind the same `Tool` trait later.
Every call passes a deterministic `PermissionGate` (otto's guardrail) before dispatch:
`DefaultPermissionGate` denies sensitive paths (`.env`, `.ssh/`, `.git/`) as an inviolable
floor. An `Ask` verdict is resolved by an `AskResolver` — `DenyAsk` in headless mode; the
interactive UI supplies a prompting resolver later.
```

- [ ] **Step 2: Final gate**

Run: `cargo fmt --all -- --check` (clean), `cargo clippy --workspace --all-targets -- -D warnings` (clean), `cargo test --workspace` (all pass — protocol 2, engine-core: router/orchestrator/tool tests, workspace 5, providers 5, router 9, tools 8, agents 3, engine 1 integration + 1 unit).

- [ ] **Step 3: Commit**

```bash
git add docs/ARCHITECTURE.md
git commit -m "docs: document the Tool seam + permission gate in ARCHITECTURE.md"
```

---

## Done — what Plan 3a delivers

Agents can now call tools through `AgentCtx::tools()`, and every call is gated by a deterministic `PermissionGate` (the guardrail) with an inviolable sensitive-path floor. The first in-process tools — `fs.read`/`fs.write`/`fs.list` — operate over the path-contained `Workspace`, and `ContextFinder` uses `fs.list` for real, proving the agent → gate → tool → workspace path end-to-end. Tools are JSON-shaped behind a trait, so the next plan slots in real MCP.

**Carried into Plan 3b/3c (designed-for):** an MCP-stdio `Tool` impl (rmcp client) behind the same `Tool` trait for external/Claude-Code servers; `mcp-grep` + `mcp-bash` with OS sandboxing (bwrap/sandbox-exec); richer permission rules + the interactive `Ask` resolver (with the UI); and the privacy/sensitivity hints flowing from tool calls into `RouteHints`.
