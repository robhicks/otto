# otto Plan 4b — Real LLM Planner & Coder Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the stub `Planner` and `Coder` with real LLM-backed agents that prompt the router, parse structured JSON output (milestones / file edits), and fall back gracefully when parsing fails — so otto generates real plans and code with a real model while staying deterministic offline.

**Architecture:** Each agent builds an instruction prompt asking the model for a specific JSON shape, calls `ctx.router().complete(...)`, and parses the response with a tolerant `extract_json` helper (handles ```json fences + surrounding prose). On any parse failure the agent returns a safe degenerate result (Planner → the goal as one milestone; Coder → no edits). A reusable `ScriptedProvider` (a deterministic "mock LLM" keyed on prompt substrings) drives the parse path in tests. The Coder's edits still flow through the orchestrator's fail-closed gate (Plan 4a) before being written.

**Tech Stack:** Rust (edition 2024), serde + serde_json, async-trait, anyhow, tempfile (dev).

---

## Context for the implementer (read once)

Current state (`main`):
- `crates/agents/src/lib.rs` defines four agents directly: `StubPlanner` (goal → one milestone), `StubContextFinder` (lists files via `fs.list` tool — KEEP, real version is a later plan), `EchoCoder` (echoes the router completion into `otto_output.txt`), `StubVerifier` (KEEP). Agents implement `otto_engine_core::traits::Agent` with `run(&self, req: AgentRequest, ctx: &AgentCtx)` (elided lifetime — NEVER `<'_>` in `impl Agent`, or E0195). They call `ctx.router().complete(CompleteRequest { prompt }, RouteHints { .. })`.
- `AgentRequest::Plan { goal }` / `FindContext { goal }` / `Code { goal, context: Vec<PathBuf> }` / `Verify`. `AgentOutput::Plan { milestones: Vec<Milestone> }` / `Context { files }` / `Code { edits: Vec<Edit> }` / `Verify { ok, detail }`. `Milestone { description: String }`. `Edit { path: PathBuf, new_contents: String }`.
- `crates/engine/src/lib.rs` `build_default_registry()` registers `StubPlanner`→Planner, `StubContextFinder`→ContextFinder, `EchoCoder`→Coder, `StubVerifier`→Verifier.
- `crates/engine/tests/turn.rs` runs a full turn with a `SingleProviderRouter` over `LocalProvider` and asserts `otto_output.txt` contains "add a greeting" (currently written by `EchoCoder` echoing the prompt). This test WILL be updated to drive a `ScriptedProvider` returning real edit JSON.
- `otto-providers` has `LocalProvider` (deterministic echo), `OllamaProvider`, `AnthropicProvider`, in `local.rs`/`ollama.rs`/`anthropic.rs` with `lib.rs` re-exporting.
- The orchestrator already gates every Coder edit through the permission gate (Plan 4a, fail-closed): a Coder edit to a sensitive path is logged + skipped.

**Behavior change (intended):** with no LLM configured, `build_router()` uses `LocalProvider` (echo, not JSON), so the real Planner falls back to "goal as one milestone" and the real Coder falls back to "no edits". `otto run "<goal>"` offline therefore completes a turn but writes nothing — the honest result (otto needs an LLM to generate code). Tests prove the real path with `ScriptedProvider`.

**Conventions:** stay on branch `feat/plan-4b-llm-agents`; never detach HEAD; `git add`+`commit` only (no `--amend`); no AI/Claude self-attribution; per-package then workspace gates; `clippy -D warnings` clean (scope test-only imports into the test module; remove imports that become unused when stub agents are deleted); TDD.

---

## File Structure

```
crates/
├── providers/src/
│   ├── scripted.rs   # NEW: ScriptedProvider (deterministic prompt-keyed mock LLM)
│   └── lib.rs        # MODIFY: re-export ScriptedProvider
├── agents/
│   ├── Cargo.toml    # MODIFY: add serde (derive) dep
│   └── src/
│       ├── parse.rs    # NEW: extract_json (tolerant JSON extraction from completions)
│       ├── planner.rs  # NEW: real Planner (prompt → parse milestones → fallback)
│       ├── coder.rs    # NEW: real Coder (prompt → parse edits → fallback)
│       └── lib.rs      # MODIFY: module root; remove StubPlanner/EchoCoder; keep Stub ContextFinder/Verifier; re-export Planner/Coder
└── engine/
    ├── src/lib.rs      # MODIFY: build_default_registry uses Planner/Coder
    └── tests/turn.rs   # MODIFY: drive a ScriptedProvider returning plan+code JSON
```

---

## Task 1: `ScriptedProvider` — deterministic prompt-keyed mock LLM

**Files:**
- Create: `crates/providers/src/scripted.rs`
- Modify: `crates/providers/src/lib.rs`

- [ ] **Step 1: Write scripted.rs with a test**

Create `crates/providers/src/scripted.rs`:

```rust
//! `ScriptedProvider`: a deterministic provider that returns canned responses keyed by a
//! substring of the prompt. For tests and demos of LLM-dependent code (agents that
//! prompt-and-parse). Like `LocalProvider`, it performs no network I/O.

use async_trait::async_trait;
use otto_engine_core::traits::Provider;
use otto_engine_core::types::{CompleteRequest, CompleteResponse};

/// Returns the first rule whose `needle` is found in the prompt, else `default`.
pub struct ScriptedProvider {
    rules: Vec<(String, String)>,
    default: String,
}

impl ScriptedProvider {
    pub fn new(default: impl Into<String>) -> Self {
        Self { rules: Vec::new(), default: default.into() }
    }

    /// Add a rule: if the prompt contains `needle`, return `response`. First match wins.
    pub fn on(mut self, needle: impl Into<String>, response: impl Into<String>) -> Self {
        self.rules.push((needle.into(), response.into()));
        self
    }
}

#[async_trait]
impl Provider for ScriptedProvider {
    fn id(&self) -> &str {
        "scripted"
    }

    async fn complete(&self, req: CompleteRequest) -> anyhow::Result<CompleteResponse> {
        let text = self
            .rules
            .iter()
            .find(|(needle, _)| req.prompt.contains(needle.as_str()))
            .map(|(_, resp)| resp.clone())
            .unwrap_or_else(|| self.default.clone());
        Ok(CompleteResponse { text })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn returns_matching_rule_then_default() {
        let p = ScriptedProvider::new("DEFAULT")
            .on("edits", "CODE")
            .on("milestones", "PLAN");

        let code = p
            .complete(CompleteRequest { prompt: "give me edits".into() })
            .await
            .unwrap();
        assert_eq!(code.text, "CODE");

        let plan = p
            .complete(CompleteRequest { prompt: "give me milestones".into() })
            .await
            .unwrap();
        assert_eq!(plan.text, "PLAN");

        let other = p
            .complete(CompleteRequest { prompt: "hello".into() })
            .await
            .unwrap();
        assert_eq!(other.text, "DEFAULT");
        assert_eq!(p.id(), "scripted");
    }
}
```

- [ ] **Step 2: Re-export from lib.rs**

In `crates/providers/src/lib.rs`, add the module + re-export (keep `anthropic`/`local`/`ollama`):

```rust
//! otto provider implementations (in-process libraries behind `otto_engine_core::Provider`).

pub mod anthropic;
pub mod local;
pub mod ollama;
pub mod scripted;

pub use anthropic::AnthropicProvider;
pub use local::LocalProvider;
pub use ollama::OllamaProvider;
pub use scripted::ScriptedProvider;
```

- [ ] **Step 3: Test**

Run: `cargo test -p otto-providers` (the new `returns_matching_rule_then_default` plus existing pass), `cargo clippy -p otto-providers --all-targets -- -D warnings` (clean), `cargo fmt -p otto-providers` (clean).

- [ ] **Step 4: Commit**

```bash
git add crates/providers/src/scripted.rs crates/providers/src/lib.rs
git commit -m "feat(providers): ScriptedProvider — deterministic prompt-keyed mock LLM"
```

---

## Task 2: `extract_json` — tolerant JSON extraction

**Files:**
- Modify: `crates/agents/Cargo.toml`
- Create: `crates/agents/src/parse.rs`
- Modify: `crates/agents/src/lib.rs`

- [ ] **Step 1: Add serde to agents deps**

In `crates/agents/Cargo.toml` `[dependencies]` (which already has `otto-engine-core`, `async-trait`, `anyhow`, `serde_json`), add:

```toml
serde = { workspace = true }
```

- [ ] **Step 2: Write parse.rs**

Create `crates/agents/src/parse.rs`:

```rust
//! Extract a JSON value from an LLM completion: tolerates ```json ... ``` fences and
//! surrounding prose by slicing from the first `{` to the last `}` as a fallback.

use serde::de::DeserializeOwned;

/// Parse `T` from `text`, tolerating Markdown code fences and leading/trailing prose.
pub fn extract_json<T: DeserializeOwned>(text: &str) -> anyhow::Result<T> {
    let slice =
        json_slice(text).ok_or_else(|| anyhow::anyhow!("no JSON object found in completion"))?;
    Ok(serde_json::from_str(slice)?)
}

/// Find the substring that looks like the JSON body: prefer the content of a fenced
/// ```` ``` ```` block, else the span from the first `{` to the last `}`.
fn json_slice(text: &str) -> Option<&str> {
    if let Some(fence_start) = text.find("```") {
        let after = &text[fence_start + 3..];
        // Skip the rest of the fence line (e.g. "json").
        if let Some(nl) = after.find('\n') {
            let body = &after[nl + 1..];
            if let Some(end) = body.find("```") {
                let inner = body[..end].trim();
                if !inner.is_empty() {
                    return Some(inner);
                }
            }
        }
    }
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    if end > start { Some(&text[start..=end]) } else { None }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Deserialize, PartialEq, Debug)]
    struct Demo {
        x: u32,
    }

    #[test]
    fn parses_plain_json() {
        let d: Demo = extract_json("{\"x\": 5}").unwrap();
        assert_eq!(d, Demo { x: 5 });
    }

    #[test]
    fn parses_json_in_fence_with_prose() {
        let text = "Sure!\n```json\n{\"x\": 7}\n```\nDone.";
        let d: Demo = extract_json(text).unwrap();
        assert_eq!(d, Demo { x: 7 });
    }

    #[test]
    fn parses_json_with_surrounding_prose_no_fence() {
        let d: Demo = extract_json("Here: {\"x\": 9} ok").unwrap();
        assert_eq!(d, Demo { x: 9 });
    }

    #[test]
    fn errors_when_no_json() {
        let r: anyhow::Result<Demo> = extract_json("no json here");
        assert!(r.is_err());
    }
}
```

- [ ] **Step 3: Declare the module**

In `crates/agents/src/lib.rs`, add `pub mod parse;` at the top of the file (after the `//!` doc comment, before the existing `use` lines). Do not change anything else in this task.

- [ ] **Step 4: Test**

Run: `cargo test -p otto-agents parse::` (4 tests pass), `cargo clippy -p otto-agents --all-targets -- -D warnings` (clean), `cargo fmt -p otto-agents` (clean).

- [ ] **Step 5: Commit**

```bash
git add crates/agents/Cargo.toml crates/agents/src/parse.rs crates/agents/src/lib.rs
git commit -m "feat(agents): extract_json — tolerant JSON extraction from completions"
```

---

## Task 3: Real `Planner`

**Files:**
- Create: `crates/agents/src/planner.rs`
- Modify: `crates/agents/src/lib.rs`

- [ ] **Step 1: Write planner.rs**

Create `crates/agents/src/planner.rs`:

```rust
//! The Planner agent: prompts the router to decompose a goal into milestones, parsing a
//! structured JSON response. Falls back to the whole goal as one milestone on parse failure.

use async_trait::async_trait;
use otto_engine_core::router::{RouteHints, TaskKind};
use otto_engine_core::traits::{Agent, AgentCtx};
use otto_engine_core::types::{AgentOutput, AgentRequest, CompleteRequest, Milestone};
use serde::Deserialize;

use crate::parse::extract_json;

pub struct Planner;

#[derive(Deserialize)]
struct PlanResponse {
    milestones: Vec<MilestoneDto>,
}

#[derive(Deserialize)]
struct MilestoneDto {
    description: String,
}

fn plan_prompt(goal: &str) -> String {
    format!(
        "You are otto's planner. Decompose the goal into an ordered list of concrete milestones.\n\
         Goal: {goal}\n\
         Respond ONLY with JSON of the form: {{\"milestones\": [{{\"description\": \"...\"}}]}}"
    )
}

#[async_trait]
impl Agent for Planner {
    async fn run(&self, req: AgentRequest, ctx: &AgentCtx) -> anyhow::Result<AgentOutput> {
        let AgentRequest::Plan { goal } = req else {
            anyhow::bail!("Planner received a non-Plan request");
        };
        let completion = ctx
            .router()
            .complete(
                CompleteRequest { prompt: plan_prompt(&goal) },
                RouteHints { task_kind: TaskKind::Architecture, ..RouteHints::default() },
            )
            .await?;
        // Parse the structured plan; on any failure or empty plan, fall back to the whole
        // goal as a single milestone so the turn can still proceed.
        let milestones = match extract_json::<PlanResponse>(&completion.text) {
            Ok(plan) if !plan.milestones.is_empty() => plan
                .milestones
                .into_iter()
                .map(|m| Milestone { description: m.description })
                .collect(),
            _ => vec![Milestone { description: goal }],
        };
        Ok(AgentOutput::Plan { milestones })
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

    async fn run_planner(router: &SingleProviderRouter, goal: &str) -> Vec<Milestone> {
        let dir = tempfile::tempdir().unwrap();
        let ws = LocalWorkspace::new(dir.path());
        let tools = ToolRegistry::new(Arc::new(DefaultPermissionGate::new()), Arc::new(DenyAsk));
        let ctx = AgentCtx::new(router, &ws, &tools);
        let out = Planner
            .run(AgentRequest::Plan { goal: goal.to_string() }, &ctx)
            .await
            .unwrap();
        match out {
            AgentOutput::Plan { milestones } => milestones,
            other => panic!("expected Plan, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn parses_milestones_from_json() {
        let provider = ScriptedProvider::new("{}").on(
            "milestones",
            r#"{"milestones": [{"description": "step one"}, {"description": "step two"}]}"#,
        );
        let router = SingleProviderRouter::new(Arc::new(provider));
        let milestones = run_planner(&router, "build a thing").await;
        assert_eq!(milestones.len(), 2);
        assert_eq!(milestones[0].description, "step one");
        assert_eq!(milestones[1].description, "step two");
    }

    #[tokio::test]
    async fn falls_back_to_goal_when_unparseable() {
        // LocalProvider echoes the prompt — not JSON — so the planner falls back.
        let router = SingleProviderRouter::new(Arc::new(LocalProvider::new()));
        let milestones = run_planner(&router, "ship it").await;
        assert_eq!(milestones.len(), 1);
        assert_eq!(milestones[0].description, "ship it");
    }
}
```

- [ ] **Step 2: Update lib.rs — declare planner module, re-export, remove StubPlanner**

In `crates/agents/src/lib.rs`:
1. Add `pub mod planner;` near the other `pub mod` declarations.
2. Add `pub use planner::Planner;` (re-export).
3. DELETE the `StubPlanner` struct + its `impl Agent` block.
4. DELETE the `planner_produces_one_milestone_from_goal` test from the lib.rs test module (it tested `StubPlanner`; the planner's own tests now live in `planner.rs`).
5. Fix imports: after removing `StubPlanner`, some top-level imports may become unused. Run clippy and remove whatever it flags (likely `Milestone` if nothing else in lib.rs uses it). Keep imports still used by `StubContextFinder`/`EchoCoder`/`StubVerifier` (EchoCoder still exists until Task 4).

- [ ] **Step 3: Test**

Run: `cargo test -p otto-agents` (the planner tests `parses_milestones_from_json` + `falls_back_to_goal_when_unparseable` pass; the remaining lib.rs tests `coder_turns_completion_into_an_edit` + `context_finder_lists_workspace_files_through_tools` still pass), `cargo clippy -p otto-agents --all-targets -- -D warnings` (clean), `cargo fmt -p otto-agents` (clean). (Do NOT run `cargo test --workspace` yet — the engine still references `StubPlanner` via `build_default_registry` and won't compile until Task 5.)

- [ ] **Step 4: Commit**

```bash
git add crates/agents
git commit -m "feat(agents): real Planner — prompt + parse milestones with goal fallback"
```

---

## Task 4: Real `Coder`

**Files:**
- Create: `crates/agents/src/coder.rs`
- Modify: `crates/agents/src/lib.rs`

- [ ] **Step 1: Write coder.rs**

Create `crates/agents/src/coder.rs`:

```rust
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

fn code_prompt(goal: &str, context: &[PathBuf]) -> String {
    let files = if context.is_empty() {
        "(none)".to_string()
    } else {
        context
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    };
    format!(
        "You are otto's coder. Produce the complete file edits that accomplish the goal.\n\
         Goal: {goal}\n\
         Existing files: {files}\n\
         Respond ONLY with JSON of the form: \
         {{\"edits\": [{{\"path\": \"relative/path\", \"contents\": \"full new file contents\"}}]}}"
    )
}

#[async_trait]
impl Agent for Coder {
    async fn run(&self, req: AgentRequest, ctx: &AgentCtx) -> anyhow::Result<AgentOutput> {
        let AgentRequest::Code { goal, context } = req else {
            anyhow::bail!("Coder received a non-Code request");
        };
        let completion = ctx
            .router()
            .complete(
                CompleteRequest { prompt: code_prompt(&goal, &context) },
                RouteHints { task_kind: TaskKind::Edit, ..RouteHints::default() },
            )
            .await?;
        // Parse the edits; on any failure produce no edits.
        let edits = match extract_json::<CodeResponse>(&completion.text) {
            Ok(code) => code
                .edits
                .into_iter()
                .map(|e| Edit { path: PathBuf::from(e.path), new_contents: e.contents })
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

    async fn run_coder(router: &SingleProviderRouter) -> Vec<Edit> {
        let dir = tempfile::tempdir().unwrap();
        let ws = LocalWorkspace::new(dir.path());
        let tools = ToolRegistry::new(Arc::new(DefaultPermissionGate::new()), Arc::new(DenyAsk));
        let ctx = AgentCtx::new(router, &ws, &tools);
        let out = Coder
            .run(
                AgentRequest::Code { goal: "add a greeting".to_string(), context: Vec::new() },
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
        let edits = run_coder(&router).await;
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].path, PathBuf::from("greeting.txt"));
        assert_eq!(edits[0].new_contents, "hello world");
    }

    #[tokio::test]
    async fn falls_back_to_no_edits_when_unparseable() {
        let router = SingleProviderRouter::new(Arc::new(LocalProvider::new()));
        let edits = run_coder(&router).await;
        assert!(edits.is_empty());
    }
}
```

- [ ] **Step 2: Update lib.rs — declare coder module, re-export, remove EchoCoder**

In `crates/agents/src/lib.rs`:
1. Add `pub mod coder;` near the other `pub mod` declarations.
2. Add `pub use coder::Coder;`.
3. DELETE the `EchoCoder` struct + its `impl Agent` block.
4. DELETE the `coder_turns_completion_into_an_edit` test from the lib.rs test module.
5. Fix imports: after removing `EchoCoder`, remove now-unused top-level imports. After Tasks 3+4, `lib.rs` should only define `StubContextFinder` and `StubVerifier` plus the `context_finder_lists_workspace_files_through_tools` test. The needed imports are roughly: `async_trait::async_trait`, `otto_engine_core::traits::{Agent, AgentCtx}`, `otto_engine_core::types::{AgentOutput, AgentRequest}`, `serde_json::Value`. Remove `PathBuf`, `RouteHints`, `TaskKind`, `CompleteRequest`, `Edit`, `Milestone` if unused. Run clippy `-D warnings` and resolve every unused-import warning.

- [ ] **Step 3: Test**

Run: `cargo test -p otto-agents` (coder tests `parses_edits_from_json` + `falls_back_to_no_edits_when_unparseable` pass; planner tests pass; `context_finder_lists_workspace_files_through_tools` passes), `cargo clippy -p otto-agents --all-targets -- -D warnings` (clean), `cargo fmt -p otto-agents` (clean). (Still do NOT run `--workspace` — engine references `EchoCoder`/`StubPlanner` until Task 5.)

- [ ] **Step 4: Commit**

```bash
git add crates/agents
git commit -m "feat(agents): real Coder — prompt + parse edits with no-edit fallback"
```

---

## Task 5: Engine wiring + integration test

**Files:**
- Modify: `crates/engine/src/lib.rs`
- Modify: `crates/engine/tests/turn.rs`

- [ ] **Step 1: Register the real agents**

In `crates/engine/src/lib.rs`, change the agents import and `build_default_registry`. The import line `use otto_agents::{EchoCoder, StubContextFinder, StubPlanner, StubVerifier};` becomes:

```rust
use otto_agents::{Coder, Planner, StubContextFinder, StubVerifier};
```

And `build_default_registry` becomes:

```rust
/// Build the registry of built-in agents: real LLM-backed Planner + Coder, plus the
/// (still-stub) ContextFinder and Verifier.
pub fn build_default_registry() -> AgentRegistry {
    let mut registry = AgentRegistry::new();
    registry.register(Role::Planner, Arc::new(Planner));
    registry.register(Role::ContextFinder, Arc::new(StubContextFinder));
    registry.register(Role::Coder, Arc::new(Coder));
    registry.register(Role::Verifier, Arc::new(StubVerifier));
    registry
}
```

- [ ] **Step 2: Update the integration test to drive a ScriptedProvider**

In `crates/engine/tests/turn.rs`, replace the `LocalProvider` with a `ScriptedProvider` that returns plan JSON for the planner prompt and edit JSON for the coder prompt. Update the imports and the router construction. The full file:

```rust
//! End-to-end: a full turn drives the real Planner + Coder against a scripted model that
//! returns structured JSON, writes the parsed edit into the workspace, and emits a sequenced
//! event stream ending in a successful TurnComplete.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use otto_engine::{build_tool_registry, run_goal};
use otto_engine_core::traits::Workspace;
use otto_protocol::EventKind;
use otto_providers::ScriptedProvider;
use otto_router::SingleProviderRouter;
use otto_workspace::LocalWorkspace;

#[tokio::test]
async fn full_turn_writes_parsed_edit_and_completes_ok() {
    let dir = tempfile::tempdir().unwrap();

    // A scripted model: the planner prompt contains "milestones", the coder prompt contains
    // "edits". First matching rule wins, so list "edits" first (the coder prompt does not
    // contain "milestones", and the planner prompt does not contain "edits").
    let provider = ScriptedProvider::new("{}")
        .on(
            "edits",
            r#"{"edits": [{"path": "otto_output.txt", "contents": "Hello! add a greeting"}]}"#,
        )
        .on(
            "milestones",
            r#"{"milestones": [{"description": "write the greeting"}]}"#,
        );
    let router = SingleProviderRouter::new(Arc::new(provider));
    let workspace = LocalWorkspace::new(dir.path());

    let tools_workspace: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(dir.path()));
    let tools = build_tool_registry(tools_workspace, dir.path().to_path_buf());

    let (events, outcome) = run_goal("add a greeting", &router, &workspace, &tools)
        .await
        .unwrap();

    assert!(outcome.ok);
    assert_eq!(
        events.last().unwrap().kind,
        EventKind::TurnComplete { ok: true }
    );

    for (i, event) in events.iter().enumerate() {
        assert_eq!(event.seq, i as u64);
    }

    // The Coder's PARSED edit was applied (it passed the gate — otto_output.txt is not sensitive).
    let written = workspace.read(Path::new("otto_output.txt")).await.unwrap();
    let text = String::from_utf8(written).unwrap();
    assert!(text.contains("add a greeting"));

    assert!(events.iter().any(|e| matches!(
        &e.kind,
        EventKind::FileEdit { path, .. } if path == &PathBuf::from("otto_output.txt")
    )));
}
```

(The test fn is renamed to `full_turn_writes_parsed_edit_and_completes_ok` to reflect that the edit now comes from parsed JSON, not an echo.)

- [ ] **Step 3: Full workspace test + CLI smoke**

Run: `cargo test --workspace` (ALL pass, including the updated integration test). `cargo clippy --workspace --all-targets -- -D warnings` (clean), `cargo fmt --all -- --check` (clean).

Smoke (offline, no LLM → honest no-op): `mkdir -p /tmp/otto-p4b && cargo run -p otto-engine -- run "add a greeting" --root /tmp/otto-p4b`. Expected: the turn runs (Planner falls back to one milestone, Coder falls back to no edits), prints the event stream ending `turn ok = true`, and writes NO file (no `otto_output.txt` in /tmp/otto-p4b — confirm with `ls /tmp/otto-p4b`). This is the intended honest offline behavior. (To see real codegen: `ANTHROPIC_API_KEY=… cargo run -p otto-engine -- run "…" --root …`.)

- [ ] **Step 4: Commit**

```bash
git add crates/engine
git commit -m "feat(engine): wire real Planner + Coder; integration test drives a scripted model"
```

---

## Task 6: Docs + quality gate

**Files:**
- Modify: `docs/ARCHITECTURE.md`

- [ ] **Step 1: Document the real agents**

In `docs/ARCHITECTURE.md`, find the `### \`Agent\` — the atomic-agent seam` subsection. Append:

```markdown
The built-in `Planner` and `Coder` are real LLM agents: each builds an instruction prompt
asking the router for a specific JSON shape (`{"milestones":[…]}` / `{"edits":[…]}`), calls
`ctx.router().complete(...)`, and parses the response with `extract_json` (tolerant of ```json
fences + prose). On parse failure they degrade safely — the Planner treats the whole goal as
one milestone, the Coder produces no edits — so an offline run (no LLM configured) completes a
turn but writes nothing. Tests drive a `ScriptedProvider` (a deterministic prompt-keyed mock
LLM) to exercise the real parse path. The Coder's edits still pass the orchestrator's
fail-closed permission gate before being written. `ContextFinder` and `Verifier` remain stubs
(real versions land next).
```

- [ ] **Step 2: Final gate**

Run: `cargo fmt --all -- --check` (clean), `cargo clippy --workspace --all-targets -- -D warnings` (clean), `cargo test --workspace` — capture the per-crate breakdown and the summed total.

- [ ] **Step 3: Commit**

```bash
git add docs/ARCHITECTURE.md
git commit -m "docs: document real LLM-backed Planner and Coder agents"
```

---

## Done — what Plan 4b delivers

otto's Planner and Coder are real LLM agents: they prompt the router for structured JSON and parse it, so with a real model (`ANTHROPIC_API_KEY=…` or `OTTO_OLLAMA=1`) otto generates an actual plan and real file edits — and those edits flow through the fail-closed permission gate before being written. Offline (no LLM) the agents degrade safely and the turn writes nothing (the honest result). A reusable `ScriptedProvider` makes the LLM path deterministically testable.

**Carried forward / deferred:** real `ContextFinder` (AST import-trace + git + grep + retrieval) and real `Verifier` (run `cargo test`/build via the sandboxed `bash` tool) + the orchestrator **Repair loop** (retry the Coder on verify failure, incrementing `RouteHints.prior_failures` to drive Brain-Blend escalation). Also tracked (from Plan 4a): non-transactional partial edit application, and a `denied_edits` count on `TurnOutcome` so callers don't read success solely from `ok`.
