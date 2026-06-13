# otto Plan 2 — Real Providers & Brain-Blend Router Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Introduce a `Router` seam that selects an LLM provider per request (otto's "Brain-Blend"), wire it through the agent-facing `AgentCtx`, and add real network-backed providers (Ollama for local, Anthropic for remote) behind the existing `Provider` trait — all while keeping the full turn deterministically testable offline.

**Architecture:** Builds additively on Plan 1's seams. A new `Router` trait in `engine-core` becomes what agents call (via `AgentCtx`) instead of a single `Provider`; the orchestrator carries a `&dyn Router`. A new `otto-router` crate provides `SingleProviderRouter` (pass-through) and `BrainBlendRouter` (privacy-forced-local + complexity-scored + fallback-chain selection over a provider pool). `otto-providers` gains `OllamaProvider` and `AnthropicProvider` (reqwest HTTP clients) tested against `wiremock` mock servers so no live API keys are needed in CI. This also implements the architecture review's two "fix-now" items: `AgentCtx` gets private fields + a constructor, and agents bind to a `Router` indirection rather than one fixed provider.

**Tech Stack:** Rust (edition 2024), tokio, async-trait, anyhow, serde/serde_json, reqwest 0.12 (json, rustls-tls), wiremock 0.6 (dev), tempfile (dev).

---

## Context for the implementer (read once)

Plan 1 shipped these seams in `engine-core` (all on `main`):
- `traits.rs`: `Provider` (`fn id(&self) -> &str`, `async fn complete(&self, CompleteRequest) -> anyhow::Result<CompleteResponse>`), `Workspace`, `Agent` (`async fn run(&self, AgentRequest, ctx: &AgentCtx) -> anyhow::Result<AgentOutput>`), and `AgentCtx<'a> { pub provider: &'a dyn Provider, pub workspace: &'a dyn Workspace }`.
- `types.rs`: `CompleteRequest { prompt: String }`, `CompleteResponse { text: String }`, `Edit`, `Milestone`, `AgentRequest`, `AgentOutput`.
- `orchestrator.rs`: `Orchestrator<'a> { registry, provider: &'a dyn Provider, workspace: &'a dyn Workspace }`, `run_turn`, `Emitter`, `TurnOutcome`. The orchestrator passes a fresh `AgentCtx` into each agent.
- `registry.rs`: `AgentRegistry`.

`otto-agents`: `StubPlanner`, `StubContextFinder`, `EchoCoder` (the only agent that calls the provider: `ctx.provider.complete(...)`), `StubVerifier`.
`otto-providers`: `LocalProvider` (deterministic, no network).
`otto-engine`: `build_default_registry()`, `run_goal(goal, provider: &dyn Provider, workspace: &dyn Workspace) -> (Vec<Event>, TurnOutcome)` (stamps monotonic `seq`), `otto run` CLI.

**Critical conventions carried from Plan 1:**
- `Agent::run` takes `ctx: &AgentCtx` with an ELIDED lifetime. In any `impl Agent`, write `ctx: &AgentCtx` (NOT `&AgentCtx<'_>`) or it fails to compile (E0195).
- Every commit message uses no AI/Claude self-attribution (no Co-Authored-By, no "Generated with", no emoji).
- Per-package `cargo test -p <crate>` / `cargo clippy -p <crate> --all-targets -- -D warnings` / `cargo fmt`. Final gate runs the whole workspace.
- Git hygiene: stay on branch `feat/plan-2-providers-router`. NEVER `git checkout <sha>` / detach HEAD. Use `git add` + `git commit` only (no `--amend` across tasks).

---

## File Structure

```
crates/
├── engine-core/src/
│   ├── router.rs        # NEW: Router trait, RouteHints, TaskKind
│   ├── traits.rs        # MODIFY: AgentCtx → private fields {router, workspace} + constructor/accessors
│   ├── orchestrator.rs  # MODIFY: Orchestrator holds `router: &dyn Router` (not provider); builds ctx via AgentCtx::new; supplies RouteHints
│   └── lib.rs           # MODIFY: export router items
├── router/              # NEW CRATE: otto-router
│   ├── Cargo.toml
│   └── src/lib.rs       # SingleProviderRouter + BrainBlendRouter + RoutingPolicy
├── providers/src/
│   ├── lib.rs           # MODIFY: re-export modules; keep LocalProvider
│   ├── local.rs         # MOVE: LocalProvider into its own module
│   ├── ollama.rs        # NEW: OllamaProvider (reqwest → /api/generate)
│   └── anthropic.rs     # NEW: AnthropicProvider (reqwest → /v1/messages, configurable base_url)
├── agents/src/lib.rs    # MODIFY: EchoCoder uses ctx.router(); tests build a router
└── engine/src/
    ├── lib.rs           # MODIFY: run_goal takes `&dyn Router`; build_router(); seq-stamping unchanged
    └── main.rs          # MODIFY: build a router from env/capabilities; default deterministic
```

**Responsibility boundaries:** `engine-core` owns the `Router` *trait* (the seam) but no concrete router. `otto-router` owns concrete routers + policy and depends on `engine-core` only. `otto-providers` owns provider impls (in-process libs). `engine` is still the only crate wiring concretes together. Add `crates/router` to the workspace `members` list.

---

## Task 1: `Router` trait + routing inputs in engine-core

**Files:**
- Create: `crates/engine-core/src/router.rs`
- Modify: `crates/engine-core/src/lib.rs`

- [ ] **Step 1: Write the trait + types with a unit test**

Create `crates/engine-core/src/router.rs`:

```rust
//! The routing seam. Agents call a `Router` (not a single `Provider`) so the engine
//! can pick local vs remote per request — otto's "Brain-Blend". `engine-core` owns the
//! trait; concrete routers live in the `otto-router` crate.

use async_trait::async_trait;

use crate::types::{CompleteRequest, CompleteResponse};

/// The kind of work a request represents. Influences local-vs-remote routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TaskKind {
    /// Mechanical, low-stakes generation. Cheapest tier; prefers local.
    #[default]
    Boilerplate,
    /// A focused code edit. Mid tier.
    Edit,
    /// Cross-cutting or design-level reasoning. Prefers a frontier remote model.
    Architecture,
}

/// Inputs the orchestrator/agents supply to influence routing. All optional-ish with
/// sensible defaults so callers only set what they know.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RouteHints {
    pub task_kind: TaskKind,
    /// Rough size of the request context, in tokens. 0 if unknown.
    pub token_estimate: usize,
    /// If true, the request touches sensitive data and MUST stay local.
    pub privacy_sensitive: bool,
    /// How many times this logical step has already failed. Drives escalation.
    pub prior_failures: u32,
}

/// Selects a provider per request and runs the completion. The agent-facing seam.
#[async_trait]
pub trait Router: Send + Sync {
    async fn complete(
        &self,
        req: CompleteRequest,
        hints: RouteHints,
    ) -> anyhow::Result<CompleteResponse>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_hints_default_is_boilerplate_and_zeroed() {
        let h = RouteHints::default();
        assert_eq!(h.task_kind, TaskKind::Boilerplate);
        assert_eq!(h.token_estimate, 0);
        assert!(!h.privacy_sensitive);
        assert_eq!(h.prior_failures, 0);
    }
}
```

- [ ] **Step 2: Export from lib.rs**

In `crates/engine-core/src/lib.rs`, add the module declaration and re-exports. Add `pub mod router;` alongside the existing `pub mod` lines, and extend the re-exports:

```rust
pub mod orchestrator;
pub mod registry;
pub mod router;
pub mod traits;
pub mod types;

pub use orchestrator::{Emitter, Orchestrator, TurnOutcome};
pub use registry::AgentRegistry;
pub use router::{RouteHints, Router, TaskKind};
pub use traits::{Agent, AgentCtx, Provider, Workspace};
pub use types::{
    AgentOutput, AgentRequest, CompleteRequest, CompleteResponse, Edit, Milestone,
};
```

- [ ] **Step 3: Run the test**

Run: `cargo test -p otto-engine-core router::`
Expected: PASS (`route_hints_default_is_boilerplate_and_zeroed`). The crate still compiles because nothing yet consumes `Router`.

- [ ] **Step 4: fmt/clippy**

Run: `cargo fmt -p otto-engine-core` and `cargo clippy -p otto-engine-core --all-targets -- -D warnings`. Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add crates/engine-core/src/router.rs crates/engine-core/src/lib.rs
git commit -m "feat(engine-core): add Router seam (Router trait, RouteHints, TaskKind)"
```

---

## Task 2: `otto-router` crate with `SingleProviderRouter`

**Files:**
- Create: `crates/router/Cargo.toml`
- Create: `crates/router/src/lib.rs`
- Modify: `Cargo.toml` (workspace members)

- [ ] **Step 1: Add the crate to the workspace**

In the root `Cargo.toml`, add `"crates/router"` to the `members` array (place it after `"crates/providers"`):

```toml
members = [
    "crates/protocol",
    "crates/engine-core",
    "crates/workspace",
    "crates/providers",
    "crates/router",
    "crates/agents",
    "crates/engine",
]
```

- [ ] **Step 2: Create the crate manifest**

Create `crates/router/Cargo.toml`:

```toml
[package]
name = "otto-router"
version = "0.1.0"
edition.workspace = true
rust-version.workspace = true
license.workspace = true

[dependencies]
otto-engine-core = { path = "../engine-core" }
async-trait.workspace = true
anyhow.workspace = true

[dev-dependencies]
tokio = { workspace = true, features = ["macros", "rt-multi-thread"] }
```

- [ ] **Step 3: Write `SingleProviderRouter` with a test**

Create `crates/router/src/lib.rs`. `SingleProviderRouter` is the trivial pass-through used by tests and single-provider setups; it satisfies `Router` by always delegating to its one provider.

```rust
//! Concrete routers for otto. `SingleProviderRouter` is a pass-through over one
//! provider; `BrainBlendRouter` (added later) selects across a pool.

use std::sync::Arc;

use async_trait::async_trait;
use otto_engine_core::router::{RouteHints, Router};
use otto_engine_core::traits::Provider;
use otto_engine_core::types::{CompleteRequest, CompleteResponse};

/// A router that always delegates to a single provider, ignoring hints. Used for
/// deterministic tests and setups where only one provider is configured.
pub struct SingleProviderRouter {
    provider: Arc<dyn Provider>,
}

impl SingleProviderRouter {
    pub fn new(provider: Arc<dyn Provider>) -> Self {
        Self { provider }
    }
}

#[async_trait]
impl Router for SingleProviderRouter {
    async fn complete(
        &self,
        req: CompleteRequest,
        _hints: RouteHints,
    ) -> anyhow::Result<CompleteResponse> {
        self.provider.complete(req).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct EchoProvider;
    #[async_trait]
    impl Provider for EchoProvider {
        fn id(&self) -> &str {
            "echo"
        }
        async fn complete(&self, req: CompleteRequest) -> anyhow::Result<CompleteResponse> {
            Ok(CompleteResponse { text: format!("echo:{}", req.prompt) })
        }
    }

    #[tokio::test]
    async fn single_provider_router_delegates() {
        let router = SingleProviderRouter::new(Arc::new(EchoProvider));
        let out = router
            .complete(CompleteRequest { prompt: "hi".into() }, RouteHints::default())
            .await
            .unwrap();
        assert_eq!(out.text, "echo:hi");
    }
}
```

- [ ] **Step 4: Run the test**

Run: `cargo test -p otto-router`
Expected: PASS (`single_provider_router_delegates`).

- [ ] **Step 5: fmt/clippy**

Run: `cargo fmt -p otto-router` and `cargo clippy -p otto-router --all-targets -- -D warnings`. Expected: clean.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml crates/router
git commit -m "feat(router): otto-router crate with SingleProviderRouter pass-through"
```

---

## Task 3: Reshape `AgentCtx` and `Orchestrator` to the Router seam

**Files:**
- Modify: `crates/engine-core/src/traits.rs`
- Modify: `crates/engine-core/src/orchestrator.rs`

This is the keystone refactor. `AgentCtx` stops exposing a `Provider` and instead exposes a `Router`, with private fields + a constructor + accessors (the architecture review's "fix-now" item). The orchestrator holds a `&dyn Router` and passes `RouteHints` appropriate to each phase.

- [ ] **Step 1: Reshape `AgentCtx` in traits.rs**

In `crates/engine-core/src/traits.rs`, change the imports and the `AgentCtx` definition. Replace the existing `use crate::types::...` line and the `AgentCtx` struct with:

```rust
use crate::router::Router;
use crate::types::{AgentOutput, AgentRequest, CompleteRequest, CompleteResponse, Edit};
```

(Keep the `Provider`, `Workspace`, `Agent` trait definitions exactly as they are — `Provider` stays in the crate; it is still implemented by concrete providers and held by routers.)

Replace the `AgentCtx` struct (the old `pub struct AgentCtx<'a> { pub provider: ..., pub workspace: ... }`) with:

```rust
/// Scoped resources an agent may use during a turn. Fields are private; construct via
/// `new` and read via accessors so capabilities can be added without breaking callers.
pub struct AgentCtx<'a> {
    router: &'a dyn Router,
    workspace: &'a dyn Workspace,
}

impl<'a> AgentCtx<'a> {
    pub fn new(router: &'a dyn Router, workspace: &'a dyn Workspace) -> Self {
        Self { router, workspace }
    }

    /// The router agents call to run completions (local-vs-remote selection happens inside).
    pub fn router(&self) -> &dyn Router {
        self.router
    }

    /// The workspace agents read from / write edits to.
    pub fn workspace(&self) -> &dyn Workspace {
        self.workspace
    }
}
```

Note: `CompleteRequest`/`CompleteResponse` remain imported because the `Provider` trait signature uses them. `Edit`, `AgentRequest`, `AgentOutput` remain used by `Workspace`/`Agent`.

- [ ] **Step 2: Update `Orchestrator` to hold a router**

In `crates/engine-core/src/orchestrator.rs`, change the imports and struct. Replace `use crate::traits::{AgentCtx, Provider, Workspace};` with:

```rust
use crate::router::{RouteHints, Router, TaskKind};
use crate::traits::{AgentCtx, Workspace};
```

Replace the `Orchestrator` struct definition:

```rust
pub struct Orchestrator<'a> {
    pub registry: &'a AgentRegistry,
    pub router: &'a dyn Router,
    pub workspace: &'a dyn Workspace,
}
```

In `run_turn`, replace the `let ctx = AgentCtx { provider: self.provider, workspace: self.workspace };` line with:

```rust
        let ctx = AgentCtx::new(self.router, self.workspace);
```

Everything else in `run_turn` is unchanged (the agents receive `&ctx` exactly as before). `RouteHints`/`TaskKind`/`Router` are imported so the in-crate test fakes can use them; `RouteHints` and `TaskKind` may be unused in the non-test body — if clippy flags an unused import, scope the import into the test module instead (`use crate::router::{RouteHints, Router}` in the body if only `Router` is used there, and `use super::*` already covers the test module). Adjust to whatever compiles clean under `-D warnings`: the body needs only `Router` and `Workspace`; the tests need `RouteHints`.

- [ ] **Step 3: Update the orchestrator's inline tests to use a fake Router**

In the `#[cfg(test)] mod tests` of `orchestrator.rs`, the `FakeProvider` must be replaced by a fake **Router** (the orchestrator no longer takes a provider). Replace the `FakeProvider` struct and its impl with:

```rust
    struct FakeRouter;
    #[async_trait]
    impl Router for FakeRouter {
        async fn complete(
            &self,
            _req: CompleteRequest,
            _hints: RouteHints,
        ) -> anyhow::Result<CompleteResponse> {
            Ok(CompleteResponse { text: "fake".to_string() })
        }
    }
```

Update the test imports inside the module: ensure `use crate::router::{RouteHints, Router};` and `use crate::types::{CompleteRequest, CompleteResponse, Edit, Milestone};` are present (add `CompleteRequest`/`CompleteResponse` if not already). In BOTH tests, change the orchestrator construction from `provider: &provider` to `router: &router`, and replace `let provider = FakeProvider;` with `let router = FakeRouter;`. So each test now reads:

```rust
        let reg = registry();
        let router = FakeRouter;
        let workspace = RecordingWorkspace::default();
        let orch = Orchestrator { registry: &reg, router: &router, workspace: &workspace };
```

(The second test `run_turn_errors_when_a_role_is_missing` gets the same `router`/construction change.) The four inline fake agents still use `_ctx: &AgentCtx` (no `<'_>`). They don't call the router, so they need no change beyond compiling against the new `AgentCtx`.

- [ ] **Step 4: Build & test engine-core**

Run: `cargo test -p otto-engine-core` — both orchestrator tests plus the router test pass.
Run: `cargo clippy -p otto-engine-core --all-targets -- -D warnings` and `cargo fmt -p otto-engine-core`. Expected: clean. (The `agents` and `engine` crates will NOT compile yet — they still reference the old `AgentCtx`/`run_goal` shapes. That's expected; Tasks 4–5 fix them. Do not run `--workspace` yet.)

- [ ] **Step 5: Commit**

```bash
git add crates/engine-core
git commit -m "refactor(engine-core): AgentCtx carries a Router (private fields + accessors); Orchestrator holds a router"
```

---

## Task 4: Update `otto-agents` to the Router seam

**Files:**
- Modify: `crates/agents/Cargo.toml`
- Modify: `crates/agents/src/lib.rs`

- [ ] **Step 1: Add otto-router as a dev-dependency**

The agent tests need a concrete `Router`. In `crates/agents/Cargo.toml`, add to `[dev-dependencies]` (keep the existing `otto-providers`, `otto-workspace`, `tokio`, `tempfile`):

```toml
otto-router = { path = "../router" }
```

- [ ] **Step 2: Update `EchoCoder` to call the router**

In `crates/agents/src/lib.rs`, update the imports to drop the now-unused `CompleteRequest` import only if it becomes unused — it is still used by `EchoCoder`, so keep it. Change the `EchoCoder::run` body. Replace the line `let completion = ctx.provider.complete(CompleteRequest { prompt: goal }).await?;` with a router call that supplies hints (an Edit task):

```rust
        let completion = ctx
            .router()
            .complete(
                CompleteRequest { prompt: goal },
                RouteHints { task_kind: TaskKind::Edit, ..RouteHints::default() },
            )
            .await?;
```

Update the `use` block at the top of the file to import the router hint types. The imports should be:

```rust
use std::path::PathBuf;

use async_trait::async_trait;
use otto_engine_core::router::{RouteHints, TaskKind};
use otto_engine_core::traits::{Agent, AgentCtx};
use otto_engine_core::types::{AgentOutput, AgentRequest, CompleteRequest, Edit, Milestone};
```

The other three agents (`StubPlanner`, `StubContextFinder`, `StubVerifier`) do not touch the context and need no change (they keep `_ctx: &AgentCtx`).

- [ ] **Step 3: Update the test `ctx` helper to build a Router**

In the `#[cfg(test)] mod tests` of `crates/agents/src/lib.rs`, the helper currently builds `AgentCtx { provider, workspace }`. Replace the helper and its usage. Change the test imports to:

```rust
    use super::*;
    use otto_providers::LocalProvider;
    use otto_router::SingleProviderRouter;
    use otto_workspace::LocalWorkspace;
    use std::sync::Arc;
```

Replace the `ctx` helper function with one that takes a router and a workspace:

```rust
    fn ctx<'a>(
        router: &'a SingleProviderRouter,
        workspace: &'a LocalWorkspace,
    ) -> AgentCtx<'a> {
        AgentCtx::new(router, workspace)
    }
```

In BOTH tests, replace `let provider = LocalProvider::new();` with a router built over a `LocalProvider`:

```rust
        let router = SingleProviderRouter::new(Arc::new(LocalProvider::new()));
```

and update the `ctx(&provider, &ws)` call sites to `ctx(&router, &ws)`. The assertions are unchanged: `EchoCoder` still produces an edit to `otto_output.txt` whose contents contain the goal (the `LocalProvider` text passes through the `SingleProviderRouter` unchanged).

- [ ] **Step 4: Test**

Run: `cargo test -p otto-agents` — both tests pass (`planner_produces_one_milestone_from_goal`, `coder_turns_completion_into_an_edit`).
Run: `cargo clippy -p otto-agents --all-targets -- -D warnings` and `cargo fmt -p otto-agents`. Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add crates/agents
git commit -m "refactor(agents): EchoCoder calls ctx.router() with RouteHints"
```

---

## Task 5: Update `otto-engine` wiring + CLI to the Router seam

**Files:**
- Modify: `crates/engine/Cargo.toml`
- Modify: `crates/engine/src/lib.rs`
- Modify: `crates/engine/src/main.rs`
- Modify: `crates/engine/tests/turn.rs`

This restores a green `cargo test --workspace` with identical end-to-end behavior, now routed through a `SingleProviderRouter`.

- [ ] **Step 1: Add otto-router dependency**

In `crates/engine/Cargo.toml` `[dependencies]`, add (keep all existing deps):

```toml
otto-router = { path = "../router" }
```

- [ ] **Step 2: Change `run_goal` to take a `&dyn Router`**

In `crates/engine/src/lib.rs`, update the imports and `run_goal` signature. Change the `use otto_engine_core::...` lines so `Router` is imported and `Provider` is dropped:

```rust
use otto_engine_core::traits::Workspace;
use otto_engine_core::{AgentRegistry, Orchestrator, Router, TurnOutcome};
use otto_protocol::{Event, EventKind, Role, SessionId};
```

Change the `run_goal` signature and the orchestrator construction. Replace `provider: &dyn Provider` with `router: &dyn Router` in the signature, and the orchestrator line from `provider` to `router`:

```rust
pub async fn run_goal(
    goal: &str,
    router: &dyn Router,
    workspace: &dyn Workspace,
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

    let orchestrator = Orchestrator { registry: &registry, router, workspace };
    let outcome = orchestrator.run_turn(session, goal, &sink).await?;

    let events = collected.lock().unwrap().clone();
    Ok((events, outcome))
}
```

`build_default_registry()` is unchanged.

- [ ] **Step 3: Update the CLI to build a router**

In `crates/engine/src/main.rs`, the CLI must now construct a router over `LocalProvider`. Update the imports and the provider/workspace construction. Change:

```rust
use otto_providers::LocalProvider;
use otto_router::SingleProviderRouter;
use otto_workspace::LocalWorkspace;
use std::sync::Arc;
```

(keep `use otto_engine::run_goal;` and `use std::path::PathBuf;`). Replace the `let provider = LocalProvider::new();` line and the `run_goal(&goal, &provider, &workspace)` call with:

```rust
    let router = SingleProviderRouter::new(Arc::new(LocalProvider::new()));
    let workspace = LocalWorkspace::new(root);

    let (events, outcome) = run_goal(&goal, &router, &workspace).await?;
```

(The `root` parsing, the event-printing loop, and the exit-code logic are unchanged.)

- [ ] **Step 4: Update the integration test**

In `crates/engine/tests/turn.rs`, build a router instead of passing a provider. Update imports:

```rust
use std::path::{Path, PathBuf};
use std::sync::Arc;

use otto_engine::run_goal;
use otto_engine_core::traits::Workspace;
use otto_protocol::EventKind;
use otto_providers::LocalProvider;
use otto_router::SingleProviderRouter;
use otto_workspace::LocalWorkspace;
```

Add `otto-router` to `crates/engine/Cargo.toml` `[dev-dependencies]` is NOT needed — it's already a normal dependency from Step 1, so the integration test can use it. Replace `let provider = LocalProvider::new();` with:

```rust
    let router = SingleProviderRouter::new(Arc::new(LocalProvider::new()));
```

and the `run_goal("add a greeting", &provider, &workspace)` call with `run_goal("add a greeting", &router, &workspace)`. All assertions stay the same (monotonic seq, file written containing "add a greeting", `FileEdit` event, `TurnComplete{ok:true}` last).

- [ ] **Step 5: Full workspace test + CLI smoke**

Run: `cargo test --workspace` — ALL tests pass (the refactor is behavior-preserving).
Run: `cargo clippy --workspace --all-targets -- -D warnings` and `cargo fmt --all -- --check`. Expected: clean.
Run: `mkdir -p /tmp/otto-p2 && cargo run -p otto-engine -- run "add a greeting" --root /tmp/otto-p2 && cat /tmp/otto-p2/otto_output.txt`. Expected: 12-event stream ending `turn ok = true`; file contains "add a greeting".

- [ ] **Step 6: Commit**

```bash
git add crates/engine
git commit -m "refactor(engine): run_goal + CLI route through SingleProviderRouter"
```

---

## Task 6: `BrainBlendRouter` — pool, policy, fallback

**Files:**
- Modify: `crates/router/src/lib.rs`

`BrainBlendRouter` holds a named provider pool with a designated *local* provider id and a *remote* provider id, and a deterministic policy: privacy-sensitive OR low complexity → local; high complexity → remote; on the chosen provider erroring, fall back to the other. All testable with deterministic in-memory providers (no network).

- [ ] **Step 1: Write the failing tests + policy + router**

Append to `crates/router/src/lib.rs` (keep `SingleProviderRouter`). Add the imports needed at the top of the file (merge with existing): add `use std::collections::HashMap;`.

```rust
/// Outcome of a routing decision: which provider id should handle a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    Local,
    Remote,
}

/// Deterministic policy mapping `RouteHints` to a `Route`. Pure function, easily tested.
pub fn decide_route(hints: &RouteHints) -> Route {
    // Privacy always forces local, regardless of complexity.
    if hints.privacy_sensitive {
        return Route::Local;
    }
    // Escalate to remote after repeated local failures.
    if hints.prior_failures >= 2 {
        return Route::Remote;
    }
    // Complexity score in [0.0, 1.0]: blend task kind and context size.
    let kind_weight = match hints.task_kind {
        TaskKind::Boilerplate => 0.0_f64,
        TaskKind::Edit => 0.4,
        TaskKind::Architecture => 1.0,
    };
    // 8k tokens saturates the size contribution.
    let size_weight = (hints.token_estimate as f64 / 8000.0).min(1.0);
    let complexity = 0.6 * kind_weight + 0.4 * size_weight;
    if complexity >= 0.5 { Route::Remote } else { Route::Local }
}

/// Brain-Blend router: a local + remote provider with privacy/complexity routing and a
/// cross-provider fallback when the primary choice errors.
pub struct BrainBlendRouter {
    providers: HashMap<Route, Arc<dyn Provider>>,
}

impl BrainBlendRouter {
    pub fn new(local: Arc<dyn Provider>, remote: Arc<dyn Provider>) -> Self {
        let mut providers = HashMap::new();
        providers.insert(Route::Local, local);
        providers.insert(Route::Remote, remote);
        Self { providers }
    }

    fn provider(&self, route: &Route) -> &Arc<dyn Provider> {
        // Both keys are always present (inserted in `new`), so this cannot fail.
        self.providers.get(route).expect("route always present")
    }

    fn other(route: &Route) -> Route {
        match route {
            Route::Local => Route::Remote,
            Route::Remote => Route::Local,
        }
    }
}

#[async_trait]
impl Router for BrainBlendRouter {
    async fn complete(
        &self,
        req: CompleteRequest,
        hints: RouteHints,
    ) -> anyhow::Result<CompleteResponse> {
        let primary = decide_route(&hints);
        match self.provider(&primary).complete(req.clone()).await {
            Ok(resp) => Ok(resp),
            Err(primary_err) => {
                // Fall back to the other provider once; if it also fails, surface both.
                let fallback = Self::other(&primary);
                self.provider(&fallback)
                    .complete(req)
                    .await
                    .map_err(|fallback_err| {
                        anyhow::anyhow!(
                            "both providers failed: primary({primary:?})={primary_err}; \
                             fallback({fallback:?})={fallback_err}"
                        )
                    })
            }
        }
    }
}
```

Now add tests to the existing `#[cfg(test)] mod tests` block. The existing `EchoProvider` returns `echo:<prompt>`; add two more fakes (a tagged provider and a failing provider) and the routing tests:

```rust
    struct TagProvider(&'static str);
    #[async_trait]
    impl Provider for TagProvider {
        fn id(&self) -> &str {
            self.0
        }
        async fn complete(&self, _req: CompleteRequest) -> anyhow::Result<CompleteResponse> {
            Ok(CompleteResponse { text: self.0.to_string() })
        }
    }

    struct FailProvider;
    #[async_trait]
    impl Provider for FailProvider {
        fn id(&self) -> &str {
            "fail"
        }
        async fn complete(&self, _req: CompleteRequest) -> anyhow::Result<CompleteResponse> {
            anyhow::bail!("boom")
        }
    }

    #[test]
    fn privacy_forces_local_even_when_complex() {
        let hints = RouteHints {
            task_kind: TaskKind::Architecture,
            token_estimate: 100_000,
            privacy_sensitive: true,
            prior_failures: 0,
        };
        assert_eq!(decide_route(&hints), Route::Local);
    }

    #[test]
    fn boilerplate_routes_local_and_architecture_routes_remote() {
        assert_eq!(decide_route(&RouteHints::default()), Route::Local);
        assert_eq!(
            decide_route(&RouteHints { task_kind: TaskKind::Architecture, ..Default::default() }),
            Route::Remote
        );
    }

    #[test]
    fn repeated_failures_escalate_to_remote() {
        let hints = RouteHints { prior_failures: 2, ..Default::default() };
        assert_eq!(decide_route(&hints), Route::Remote);
    }

    #[tokio::test]
    async fn brain_blend_routes_local_for_boilerplate() {
        let router = BrainBlendRouter::new(
            Arc::new(TagProvider("local")),
            Arc::new(TagProvider("remote")),
        );
        let out = router
            .complete(CompleteRequest { prompt: "x".into() }, RouteHints::default())
            .await
            .unwrap();
        assert_eq!(out.text, "local");
    }

    #[tokio::test]
    async fn brain_blend_falls_back_when_primary_fails() {
        // Boilerplate → primary is local; make local fail, expect remote fallback.
        let router = BrainBlendRouter::new(
            Arc::new(FailProvider),
            Arc::new(TagProvider("remote")),
        );
        let out = router
            .complete(CompleteRequest { prompt: "x".into() }, RouteHints::default())
            .await
            .unwrap();
        assert_eq!(out.text, "remote");
    }
```

- [ ] **Step 2: Test**

Run: `cargo test -p otto-router`
Expected: PASS — `single_provider_router_delegates`, `privacy_forces_local_even_when_complex`, `boilerplate_routes_local_and_architecture_routes_remote`, `repeated_failures_escalate_to_remote`, `brain_blend_routes_local_for_boilerplate`, `brain_blend_falls_back_when_primary_fails`.

- [ ] **Step 3: fmt/clippy**

Run: `cargo fmt -p otto-router` and `cargo clippy -p otto-router --all-targets -- -D warnings`. Expected: clean.

- [ ] **Step 4: Commit**

```bash
git add crates/router
git commit -m "feat(router): BrainBlendRouter with privacy/complexity routing and fallback"
```

> **Milestone:** Tasks 1–6 complete the routing architecture, fully offline-testable. The remaining tasks add real network providers. If executing incrementally, this is a natural checkpoint to ship.

---

## Task 7: `OllamaProvider` (local HTTP), reorganize providers crate

**Files:**
- Modify: `crates/providers/Cargo.toml`
- Create: `crates/providers/src/local.rs`
- Create: `crates/providers/src/ollama.rs`
- Modify: `crates/providers/src/lib.rs`

- [ ] **Step 1: Add reqwest + wiremock to the manifest**

In `crates/providers/Cargo.toml`, add to `[dependencies]` (keep `otto-engine-core`, `async-trait`, `anyhow`):

```toml
reqwest = { version = "0.12", default-features = false, features = ["json", "rustls-tls"] }
serde = { workspace = true }
serde_json = { workspace = true }
```

And to `[dev-dependencies]` (keep the existing tokio):

```toml
wiremock = "0.6"
```

- [ ] **Step 2: Move `LocalProvider` into its own module**

Create `crates/providers/src/local.rs` with the EXACT current contents of `LocalProvider` (move, don't rewrite). The file:

```rust
//! `LocalProvider`: a deterministic provider used for tests and offline runs.
//! It performs no network I/O and returns a fixed transform of the prompt.

use async_trait::async_trait;
use otto_engine_core::traits::Provider;
use otto_engine_core::types::{CompleteRequest, CompleteResponse};

/// A provider whose output is a pure function of its input — used to drive the
/// spine deterministically in CI.
pub struct LocalProvider;

impl LocalProvider {
    pub fn new() -> Self {
        Self
    }
}

impl Default for LocalProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Provider for LocalProvider {
    fn id(&self) -> &str {
        "local"
    }

    async fn complete(&self, req: CompleteRequest) -> anyhow::Result<CompleteResponse> {
        Ok(CompleteResponse {
            text: format!("// generated by otto local provider\n{}\n", req.prompt),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn complete_is_deterministic() {
        let provider = LocalProvider::new();
        let req = CompleteRequest { prompt: "add a greeting".to_string() };
        let a = provider.complete(req.clone()).await.unwrap();
        let b = provider.complete(req).await.unwrap();
        assert_eq!(a, b);
        assert!(a.text.contains("add a greeting"));
        assert_eq!(provider.id(), "local");
    }
}
```

- [ ] **Step 3: Write `OllamaProvider` with a wiremock test**

Create `crates/providers/src/ollama.rs`. It calls Ollama's `POST {base_url}/api/generate` with `{"model","prompt","stream":false}` and reads `{"response": "..."}`.

```rust
//! `OllamaProvider`: talks to a local Ollama server over HTTP (`/api/generate`).
//! Local, keyless. Default endpoint is `http://127.0.0.1:11434`.

use async_trait::async_trait;
use otto_engine_core::traits::Provider;
use otto_engine_core::types::{CompleteRequest, CompleteResponse};
use serde::{Deserialize, Serialize};

pub struct OllamaProvider {
    client: reqwest::Client,
    base_url: String,
    model: String,
}

impl OllamaProvider {
    /// `base_url` is the Ollama server root (no trailing slash), e.g. `http://127.0.0.1:11434`.
    pub fn new(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.into(),
            model: model.into(),
        }
    }

    /// Convenience constructor pointing at the default local Ollama endpoint.
    pub fn local_default(model: impl Into<String>) -> Self {
        Self::new("http://127.0.0.1:11434", model)
    }
}

#[derive(Serialize)]
struct GenerateRequest<'a> {
    model: &'a str,
    prompt: &'a str,
    stream: bool,
}

#[derive(Deserialize)]
struct GenerateResponse {
    response: String,
}

#[async_trait]
impl Provider for OllamaProvider {
    fn id(&self) -> &str {
        "ollama"
    }

    async fn complete(&self, req: CompleteRequest) -> anyhow::Result<CompleteResponse> {
        let url = format!("{}/api/generate", self.base_url);
        let body = GenerateRequest { model: &self.model, prompt: &req.prompt, stream: false };
        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json::<GenerateResponse>()
            .await?;
        Ok(CompleteResponse { text: resp.response })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn ollama_posts_generate_and_parses_response() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/generate"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "response": "hello from ollama",
                    "done": true
                })),
            )
            .mount(&server)
            .await;

        let provider = OllamaProvider::new(server.uri(), "llama3.2");
        let out = provider
            .complete(CompleteRequest { prompt: "hi".into() })
            .await
            .unwrap();
        assert_eq!(out.text, "hello from ollama");
        assert_eq!(provider.id(), "ollama");
    }

    #[tokio::test]
    async fn ollama_surfaces_http_errors() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/generate"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let provider = OllamaProvider::new(server.uri(), "llama3.2");
        let err = provider
            .complete(CompleteRequest { prompt: "hi".into() })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("500") || err.to_string().contains("status"));
    }
}
```

- [ ] **Step 4: Rewire lib.rs as a module root**

Overwrite `crates/providers/src/lib.rs` to declare the modules and re-export the providers:

```rust
//! otto provider implementations (in-process libraries behind `otto_engine_core::Provider`).

pub mod local;
pub mod ollama;

pub use local::LocalProvider;
pub use ollama::OllamaProvider;
```

- [ ] **Step 5: Test**

Run: `cargo test -p otto-providers` — `complete_is_deterministic`, `ollama_posts_generate_and_parses_response`, `ollama_surfaces_http_errors` pass.
Run: `cargo clippy -p otto-providers --all-targets -- -D warnings` and `cargo fmt -p otto-providers`. Expected: clean.

- [ ] **Step 6: Commit**

```bash
git add crates/providers
git commit -m "feat(providers): OllamaProvider (HTTP) + split LocalProvider into a module"
```

---

## Task 8: `AnthropicProvider` (remote HTTP)

**Files:**
- Create: `crates/providers/src/anthropic.rs`
- Modify: `crates/providers/src/lib.rs`

- [ ] **Step 1: Write `AnthropicProvider` with a wiremock test**

Create `crates/providers/src/anthropic.rs`. It calls `POST {base_url}/v1/messages` with the Messages API shape and reads `content[0].text`. `base_url` is configurable so tests can point at a mock; `api_base_default()` returns the real endpoint.

```rust
//! `AnthropicProvider`: talks to the Anthropic Messages API over HTTP.
//! Remote, requires an API key. `base_url` is configurable for testing.

use async_trait::async_trait;
use otto_engine_core::traits::Provider;
use otto_engine_core::types::{CompleteRequest, CompleteResponse};
use serde::{Deserialize, Serialize};

const ANTHROPIC_VERSION: &str = "2023-06-01";

pub struct AnthropicProvider {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
    max_tokens: u32,
}

impl AnthropicProvider {
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.into(),
            api_key: api_key.into(),
            model: model.into(),
            max_tokens: 4096,
        }
    }

    /// The production API base URL.
    pub fn api_base_default() -> &'static str {
        "https://api.anthropic.com"
    }
}

#[derive(Serialize)]
struct Message<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Serialize)]
struct MessagesRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    messages: Vec<Message<'a>>,
}

#[derive(Deserialize)]
struct ContentBlock {
    #[serde(default)]
    text: String,
}

#[derive(Deserialize)]
struct MessagesResponse {
    content: Vec<ContentBlock>,
}

#[async_trait]
impl Provider for AnthropicProvider {
    fn id(&self) -> &str {
        "anthropic"
    }

    async fn complete(&self, req: CompleteRequest) -> anyhow::Result<CompleteResponse> {
        let url = format!("{}/v1/messages", self.base_url);
        let body = MessagesRequest {
            model: &self.model,
            max_tokens: self.max_tokens,
            messages: vec![Message { role: "user", content: &req.prompt }],
        };
        let resp = self
            .client
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json::<MessagesResponse>()
            .await?;
        let text = resp
            .content
            .into_iter()
            .map(|b| b.text)
            .collect::<Vec<_>>()
            .join("");
        Ok(CompleteResponse { text })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn anthropic_posts_messages_with_headers_and_parses_text() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("x-api-key", "test-key"))
            .and(header("anthropic-version", ANTHROPIC_VERSION))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "content": [{ "type": "text", "text": "hello from claude" }]
                })),
            )
            .mount(&server)
            .await;

        let provider = AnthropicProvider::new(server.uri(), "test-key", "claude-haiku-4-5");
        let out = provider
            .complete(CompleteRequest { prompt: "hi".into() })
            .await
            .unwrap();
        assert_eq!(out.text, "hello from claude");
        assert_eq!(provider.id(), "anthropic");
    }

    #[tokio::test]
    async fn anthropic_surfaces_http_errors() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;

        let provider = AnthropicProvider::new(server.uri(), "bad-key", "claude-haiku-4-5");
        let err = provider
            .complete(CompleteRequest { prompt: "hi".into() })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("401") || err.to_string().contains("status"));
    }
}
```

- [ ] **Step 2: Export from lib.rs**

Update `crates/providers/src/lib.rs` to add the module and re-export:

```rust
//! otto provider implementations (in-process libraries behind `otto_engine_core::Provider`).

pub mod anthropic;
pub mod local;
pub mod ollama;

pub use anthropic::AnthropicProvider;
pub use local::LocalProvider;
pub use ollama::OllamaProvider;
```

- [ ] **Step 3: Test**

Run: `cargo test -p otto-providers` — all provider tests pass including `anthropic_posts_messages_with_headers_and_parses_text` and `anthropic_surfaces_http_errors`.
Run: `cargo clippy -p otto-providers --all-targets -- -D warnings` and `cargo fmt -p otto-providers`. Expected: clean.

- [ ] **Step 4: Commit**

```bash
git add crates/providers
git commit -m "feat(providers): AnthropicProvider (Messages API) with configurable base_url"
```

---

## Task 9: Engine wiring — build a router from the environment

**Files:**
- Modify: `crates/engine/src/lib.rs`
- Modify: `crates/engine/src/main.rs`

The engine should pick providers from the environment but keep the deterministic default so the e2e test stays offline. Policy: if `ANTHROPIC_API_KEY` is set, the remote provider is Anthropic; otherwise the remote slot reuses the local provider (so routing still functions). The local provider is Ollama if `OTTO_OLLAMA=1`, else the deterministic `LocalProvider`. When both slots are `LocalProvider`-class, behavior is deterministic.

- [ ] **Step 1: Add a `build_router` helper to lib.rs**

In `crates/engine/src/lib.rs`, add imports and a `build_router` function. Add to the `use` block:

```rust
use std::sync::Arc;

use otto_engine_core::traits::Provider;
use otto_providers::{AnthropicProvider, LocalProvider, OllamaProvider};
use otto_router::{BrainBlendRouter, SingleProviderRouter};
```

(Keep the existing imports; `Arc` may already be imported via the `std::sync::{Arc, Mutex}` line — if so, don't duplicate it.)

Add this function to the crate:

```rust
/// Build a router from environment configuration.
///
/// - Local slot: `OllamaProvider` if `OTTO_OLLAMA=1` (model from `OTTO_OLLAMA_MODEL`,
///   default `llama3.2`), otherwise the deterministic `LocalProvider`.
/// - Remote slot: `AnthropicProvider` if `ANTHROPIC_API_KEY` is set (model from
///   `OTTO_ANTHROPIC_MODEL`, default `claude-haiku-4-5`), otherwise the local slot is
///   reused so routing still works with one real backend.
///
/// With no env vars set, both slots are the deterministic `LocalProvider`, so the engine
/// runs fully offline and deterministically — the default for tests and first-run.
pub fn build_router() -> Box<dyn otto_engine_core::Router> {
    let local: Arc<dyn Provider> = if std::env::var("OTTO_OLLAMA").as_deref() == Ok("1") {
        let model = std::env::var("OTTO_OLLAMA_MODEL").unwrap_or_else(|_| "llama3.2".to_string());
        Arc::new(OllamaProvider::local_default(model))
    } else {
        Arc::new(LocalProvider::new())
    };

    match std::env::var("ANTHROPIC_API_KEY") {
        Ok(key) if !key.is_empty() => {
            let model =
                std::env::var("OTTO_ANTHROPIC_MODEL").unwrap_or_else(|_| "claude-haiku-4-5".to_string());
            let remote: Arc<dyn Provider> =
                Arc::new(AnthropicProvider::new(AnthropicProvider::api_base_default(), key, model));
            Box::new(BrainBlendRouter::new(local, remote))
        }
        _ => Box::new(SingleProviderRouter::new(local)),
    }
}
```

- [ ] **Step 2: Write a test that the default build is deterministic & offline**

Add to the `#[cfg(test)] mod tests` of `crates/engine/src/lib.rs` (create the module if absent). This test must not depend on ambient env; it asserts that with the relevant vars unset, `build_router` returns a router that drives a full turn deterministically. Because env is process-global, guard it:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use otto_engine_core::{RouteHints, Router};
    use otto_engine_core::types::CompleteRequest;

    #[tokio::test]
    async fn default_build_router_is_offline_and_deterministic() {
        // Ensure the env that would select real backends is absent for this test.
        // SAFETY: single-threaded test; we only remove vars we don't rely on elsewhere.
        unsafe {
            std::env::remove_var("OTTO_OLLAMA");
            std::env::remove_var("ANTHROPIC_API_KEY");
        }
        let router = build_router();
        let a = router
            .complete(CompleteRequest { prompt: "ping".into() }, RouteHints::default())
            .await
            .unwrap();
        let b = router
            .complete(CompleteRequest { prompt: "ping".into() }, RouteHints::default())
            .await
            .unwrap();
        assert_eq!(a, b);
        assert!(a.text.contains("ping"));
    }
}
```

Note: `std::env::remove_var` is `unsafe` in edition 2024. The `unsafe` block above is correct and required; keep it.

- [ ] **Step 3: Use `build_router` in the CLI**

In `crates/engine/src/main.rs`, replace the `let router = SingleProviderRouter::new(Arc::new(LocalProvider::new()));` line (from Task 5) with:

```rust
    let router = otto_engine::build_router();
```

Remove the now-unused imports in `main.rs` (`SingleProviderRouter`, `LocalProvider`, and `Arc` if no longer used). The `run_goal(&goal, router.as_ref(), &workspace)` call must pass the boxed router by reference — update the call to:

```rust
    let (events, outcome) = run_goal(&goal, router.as_ref(), &workspace).await?;
```

(`Box<dyn Router>` derefs to `dyn Router`; `.as_ref()` yields `&dyn Router`.)

- [ ] **Step 4: Full workspace test + CLI smoke (offline default)**

Run: `cargo test --workspace` — all pass, including `default_build_router_is_offline_and_deterministic`.
Run: `cargo clippy --workspace --all-targets -- -D warnings` and `cargo fmt --all -- --check`. Expected: clean.
Run: `mkdir -p /tmp/otto-p2b && cargo run -p otto-engine -- run "add a greeting" --root /tmp/otto-p2b && cat /tmp/otto-p2b/otto_output.txt`. Expected (no env set): deterministic turn, file contains "add a greeting".

- [ ] **Step 5: Commit**

```bash
git add crates/engine
git commit -m "feat(engine): build_router selects providers from env; deterministic offline default"
```

---

## Task 10: Workspace quality gate + docs note

**Files:**
- Modify: `docs/ARCHITECTURE.md`

- [ ] **Step 1: Note the router layer in the architecture doc**

In `docs/ARCHITECTURE.md`, under the crate layout, add a line for the new crate and the `Router` seam. Add to the crate list (after the `providers` entry):

```
│   ├── router          # SingleProviderRouter + BrainBlendRouter (privacy/complexity routing) over the Provider pool.
```

And in the "Key trait interfaces" section, add a short subsection after the `Provider` one:

```
### `Router` — the agent-facing completion seam

Agents call a `Router` (via `AgentCtx::router()`), not a single `Provider`. `engine-core`
owns the trait: `async fn complete(&self, CompleteRequest, RouteHints) -> Result<CompleteResponse>`.
`otto-router` provides `SingleProviderRouter` (pass-through) and `BrainBlendRouter`
(privacy-forced-local + complexity-scored selection over a local/remote provider pool, with
cross-provider fallback). This keeps Brain-Blend behind a stable seam: adding providers or
changing routing never touches `engine-core` or the agents.
```

- [ ] **Step 2: Final gate**

Run: `cargo fmt --all -- --check` (expected: clean).
Run: `cargo clippy --workspace --all-targets -- -D warnings` (expected: clean).
Run: `cargo test --workspace` (expected: all pass — protocol 2, engine-core 3, workspace 5, providers 5, router 6, agents 2, engine 1 integration + 1 unit).

- [ ] **Step 3: Commit**

```bash
git add docs/ARCHITECTURE.md
git commit -m "docs: document the Router layer in ARCHITECTURE.md"
```

---

## Done — what Plan 2 delivers

The agent-facing seam is now a `Router` (not a single provider), with `AgentCtx` encapsulated behind a constructor — implementing the architecture review's "fix-now" items. `BrainBlendRouter` selects local vs remote per request by privacy flag, complexity score, and failure count, with cross-provider fallback. Real `OllamaProvider` and `AnthropicProvider` HTTP clients exist behind the `Provider` trait, verified by `wiremock` so CI needs no live keys. The engine selects providers from the environment but defaults to a deterministic offline router, so the full turn and all tests remain reproducible.

**Live smoke test (manual, optional):** with Ollama running locally, `OTTO_OLLAMA=1 OTTO_OLLAMA_MODEL=llama3.2 cargo run -p otto-engine -- run "write a haiku" --root /tmp/otto-live` exercises the real local path; `ANTHROPIC_API_KEY=… cargo run …` exercises Brain-Blend with a real remote.

**Carried forward to later plans:** `RouteHints` is currently supplied only by `EchoCoder` (Edit) and defaults elsewhere; Plan 4's real agents will populate `token_estimate`/`task_kind`/`privacy_sensitive` meaningfully, and the orchestrator's Repair loop (Plan 4) will increment `prior_failures` to drive escalation. Streaming completions, an MCP tool handle, and an event emitter on `AgentCtx` remain future additions behind the now-stable constructor.
