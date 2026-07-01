# Model Routing (extensions slice 8) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make a custom command's / custom agent's parsed `model:` field take effect by pinning the remote provider's model for that turn, while preserving the privacy floor and the offline-deterministic default.

**Architecture:** A new `PinnedModelRouter` (local + remote slots) always routes remote except for privacy-sensitive requests, which stay local. A new `build_router_with_model(Option<&str>)` in the engine constructs the Anthropic remote slot with the named model id and wraps it in `PinnedModelRouter` when a key is present; with no key it warns and falls back to the offline `SingleProviderRouter`. The `--command` and `--agent` CLI paths read the def's `model` and pass it through.

**Tech Stack:** Rust (edition 2024), `async_trait`, `tokio`, `anyhow`. Crates: `otto-router`, `otto-engine` (lib + `otto` binary).

**Design spec:** `docs/superpowers/specs/2026-06-30-model-routing-design.md`

---

### Task 1: `PinnedModelRouter` in the router crate

Introduces the router that honors a pinned model by preferring remote, yielding to the privacy floor. Mirrors `BrainBlendRouter`'s structure and fallback semantics.

**Files:**
- Modify: `crates/router/src/lib.rs` (add struct + impl after `BrainBlendRouter`, ~line 127; add tests in the existing `#[cfg(test)] mod tests`)

- [ ] **Step 1: Write the failing tests**

Add these four tests inside the existing `mod tests` block in `crates/router/src/lib.rs` (the `EchoProvider` / `TagProvider` / `FailProvider` fakes already exist there — reuse them):

```rust
#[tokio::test]
async fn pinned_routes_remote_for_non_privacy() {
    // A pinned model must reach the remote slot regardless of task kind / complexity.
    let router = PinnedModelRouter::new(
        Arc::new(TagProvider("local")),
        Arc::new(TagProvider("remote")),
    );
    let out = router
        .complete(CompleteRequest { prompt: "x".into() }, RouteHints::default())
        .await
        .unwrap();
    assert_eq!(out.text, "remote");
}

#[tokio::test]
async fn pinned_routes_local_for_privacy() {
    // The privacy floor is inviolable: a privacy-sensitive request stays local even when
    // a remote model is pinned.
    let router = PinnedModelRouter::new(
        Arc::new(TagProvider("local")),
        Arc::new(TagProvider("remote")),
    );
    let hints = RouteHints { privacy_sensitive: true, ..RouteHints::default() };
    let out = router
        .complete(CompleteRequest { prompt: "secret".into() }, hints)
        .await
        .unwrap();
    assert_eq!(out.text, "local");
}

#[tokio::test]
async fn pinned_non_privacy_remote_error_falls_back_to_local() {
    // Liveness: a non-privacy remote failure falls back to local (matching BrainBlend).
    let router = PinnedModelRouter::new(Arc::new(TagProvider("local")), Arc::new(FailProvider));
    let out = router
        .complete(CompleteRequest { prompt: "x".into() }, RouteHints::default())
        .await
        .unwrap();
    assert_eq!(out.text, "local");
}

#[tokio::test]
async fn pinned_privacy_error_never_crosses_to_remote() {
    // A privacy-sensitive request routes local; if local fails it MUST surface the error,
    // never re-send to the pinned remote model.
    let router = PinnedModelRouter::new(Arc::new(FailProvider), Arc::new(TagProvider("remote")));
    let hints = RouteHints { privacy_sensitive: true, ..RouteHints::default() };
    let err = router
        .complete(CompleteRequest { prompt: "secret".into() }, hints)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("boom"), "expected local error, got: {err}");
    assert!(!err.to_string().contains("remote"), "must not have reached remote: {err}");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p otto-router pinned`
Expected: FAIL — `cannot find type PinnedModelRouter in this scope` (does not compile yet).

- [ ] **Step 3: Implement `PinnedModelRouter`**

Insert after `BrainBlendRouter`'s `impl Router` block (after line 127) in `crates/router/src/lib.rs`:

```rust
/// A router that honors an explicitly pinned remote model: it routes every request to the
/// remote provider (built with the pinned model id) EXCEPT privacy-sensitive requests, which
/// stay local — the privacy floor is inviolable. Complexity/failure escalation is ignored on
/// purpose: the caller named a model, so we honor it rather than second-guessing the tier.
pub struct PinnedModelRouter {
    local: Arc<dyn Provider>,
    remote: Arc<dyn Provider>,
}

impl PinnedModelRouter {
    pub fn new(local: Arc<dyn Provider>, remote: Arc<dyn Provider>) -> Self {
        Self { local, remote }
    }
}

#[async_trait]
impl Router for PinnedModelRouter {
    async fn complete(
        &self,
        req: CompleteRequest,
        hints: RouteHints,
    ) -> anyhow::Result<CompleteResponse> {
        // Privacy floor: a sensitive request stays local and never crosses to the remote model.
        if hints.privacy_sensitive {
            return self.local.complete(req).await;
        }
        match self.remote.complete(req.clone()).await {
            Ok(resp) => Ok(resp),
            // Non-privacy liveness fallback to local, matching BrainBlendRouter.
            Err(remote_err) => self.local.complete(req).await.map_err(|local_err| {
                anyhow::anyhow!(
                    "pinned remote failed and local fallback failed: \
                     remote={remote_err}; local={local_err}"
                )
            }),
        }
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p otto-router pinned`
Expected: PASS (4 tests).

- [ ] **Step 5: Format, lint, commit**

```bash
cargo fmt --all
cargo clippy -p otto-router --all-targets
git add crates/router/src/lib.rs
git commit -m "feat(router): PinnedModelRouter (remote-unless-privacy)"
```

---

### Task 2: `build_router_with_model` in the engine lib

Adds the model-aware router builder and refactors `build_router` into a thin wrapper. Extracts the local-slot construction so both share it.

**Files:**
- Modify: `crates/engine/src/lib.rs:16` (import `PinnedModelRouter`), `:75-97` (`build_router` → wrapper + new fn), tests block `:309`

- [ ] **Step 1: Write the failing test**

Add this test inside `crates/engine/src/lib.rs`'s `mod tests` (after `default_build_router_is_offline_and_deterministic`, before its closing `}`). It exercises the no-key + `Some(model)` graceful-fallback path — the only branch reachable without network:

```rust
#[tokio::test]
async fn model_override_without_key_is_offline_and_deterministic() {
    // SAFETY: same single-test-binary rationale as the sibling test above — no other test
    // in this binary races on these process-global env vars.
    unsafe {
        std::env::remove_var("OTTO_OLLAMA");
        std::env::remove_var("ANTHROPIC_API_KEY");
    }
    // Naming a model with no ANTHROPIC_API_KEY must NOT change routing: it falls back to the
    // offline local router (a warning is printed to stderr, not asserted here).
    let router = build_router_with_model(Some("claude-opus-4-8"));
    let a = router
        .complete(CompleteRequest { prompt: "ping".into() }, RouteHints::default())
        .await
        .unwrap();
    let b = router
        .complete(CompleteRequest { prompt: "ping".into() }, RouteHints::default())
        .await
        .unwrap();
    assert_eq!(a, b);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p otto-engine --lib model_override_without_key`
Expected: FAIL — `cannot find function build_router_with_model in this scope`.

- [ ] **Step 3: Add the import**

In `crates/engine/src/lib.rs` line 16, extend the `otto_router` use:

```rust
use otto_router::{BrainBlendRouter, PinnedModelRouter, SingleProviderRouter};
```

- [ ] **Step 4: Refactor `build_router` and add `build_router_with_model`**

Replace the whole `build_router` function (lines 75-97) with the following. `build_local_provider` factors out today's local-slot logic; `build_router_with_model` adds the pinned branch; `build_router` stays a zero-arg wrapper so existing callers are unchanged:

```rust
/// Construct the local provider slot from the environment (shared by both router builders).
fn build_local_provider() -> Arc<dyn Provider> {
    if std::env::var("OTTO_OLLAMA").as_deref() == Ok("1") {
        let model =
            std::env::var("OTTO_OLLAMA_MODEL").unwrap_or_else(|_| DEFAULT_OLLAMA_MODEL.to_string());
        Arc::new(OllamaProvider::local_default(model))
    } else {
        Arc::new(LocalProvider::new())
    }
}

pub fn build_router() -> Box<dyn otto_engine_core::Router> {
    build_router_with_model(None)
}

/// Build a router, optionally pinning the remote slot to an explicit model id (from a
/// command/agent `model:` field).
///
/// - `model_override = Some(m)` + `ANTHROPIC_API_KEY` present: the remote slot is an
///   `AnthropicProvider` built with `m` (NOT `OTTO_ANTHROPIC_MODEL`), wrapped in a
///   `PinnedModelRouter` so the named model actually runs (privacy-sensitive requests still
///   stay local).
/// - `model_override = Some(m)` + no key: not honorable — warn and fall back to the offline
///   `SingleProviderRouter`, so the default stays deterministic.
/// - `model_override = None`: unchanged behavior (`BrainBlendRouter` with a key, else
///   `SingleProviderRouter`).
pub fn build_router_with_model(model_override: Option<&str>) -> Box<dyn otto_engine_core::Router> {
    let local = build_local_provider();

    match std::env::var("ANTHROPIC_API_KEY") {
        Ok(key) if !key.is_empty() => match model_override {
            Some(model) => {
                let remote: Arc<dyn Provider> = Arc::new(AnthropicProvider::new(
                    AnthropicProvider::api_base_default(),
                    key,
                    model.to_string(),
                ));
                Box::new(PinnedModelRouter::new(local, remote))
            }
            None => {
                let model = std::env::var("OTTO_ANTHROPIC_MODEL")
                    .unwrap_or_else(|_| DEFAULT_ANTHROPIC_MODEL.to_string());
                let remote: Arc<dyn Provider> = Arc::new(AnthropicProvider::new(
                    AnthropicProvider::api_base_default(),
                    key,
                    model,
                ));
                Box::new(BrainBlendRouter::new(local, remote))
            }
        },
        _ => {
            if let Some(model) = model_override {
                eprintln!(
                    "warning: requested model '{model}' but ANTHROPIC_API_KEY is not set; \
                     falling back to the offline/local router"
                );
            }
            Box::new(SingleProviderRouter::new(local))
        }
    }
}
```

- [ ] **Step 5: Run the test and the existing sibling test**

Run: `cargo test -p otto-engine --lib build_router`
Expected: PASS — both `default_build_router_is_offline_and_deterministic` and `model_override_without_key_is_offline_and_deterministic`.

- [ ] **Step 6: Format, lint, commit**

```bash
cargo fmt --all
cargo clippy -p otto-engine --all-targets
git add crates/engine/src/lib.rs
git commit -m "feat(engine): build_router_with_model pins remote model, offline fallback"
```

---

### Task 3: Wire `--command` to the command's `model`

The command path reads `def.model` and passes it to the model-aware builder.

**Files:**
- Modify: `crates/engine/src/main.rs:493` (inside `run_command_in`)
- Test: `crates/engine/src/main.rs` `mod tests` (new test)

- [ ] **Step 1: Write the failing test**

Add to `crates/engine/src/main.rs`'s `mod tests` (near `run_command_expands_and_runs_spine`, ~line 784):

```rust
#[tokio::test]
async fn run_command_with_model_field_runs_offline() {
    use std::fs;
    // A command declaring `model:` still runs a deterministic offline turn when no
    // ANTHROPIC_API_KEY is set (graceful fallback + stderr warning).
    let proj = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap(); // empty → no user-global commands
    let cmds = proj.path().join(".claude").join("commands");
    fs::create_dir_all(&cmds).unwrap();
    fs::write(
        cmds.join("plan.md"),
        "---\nmodel: claude-opus-4-8\n---\nDescribe the plan for $1.\n",
    )
    .unwrap();

    let ok = run_command_in(
        "plan",
        &["auth".to_string()],
        proj.path().to_path_buf(),
        home.path().to_path_buf(),
    )
    .await;
    assert!(ok.is_ok(), "expected model-pinned command to run offline: {ok:?}");
}
```

- [ ] **Step 2: Run the test (regression guard, expect PASS before and after)**

Run: `cargo test -p otto-engine --bin otto run_command_with_model_field`
Expected: PASS.

This is a regression guard, not a red-green test: the model-reaches-the-router behavior is not observable on the offline path (no `ANTHROPIC_API_KEY`), so the outcome is identical with or without the Step 3 wiring. The test locks in that a `model:` field does not break the offline turn; the actual wiring is verified by the Step 3 code (`build_router_with_model(def.model.as_deref())`) plus Task 2's `PinnedModelRouter` unit coverage. Proceed to Step 3.

- [ ] **Step 3: Wire the model through**

In `crates/engine/src/main.rs`, inside `run_command_in`, change the router construction (currently line 493):

```rust
    let router: Arc<dyn otto_engine_core::Router> = Arc::from(build_router());
```

to:

```rust
    // Pin the remote model to the command's `model:` field (None = normal env-based routing).
    let router: Arc<dyn otto_engine_core::Router> =
        Arc::from(otto_engine::build_router_with_model(def.model.as_deref()));
```

(`def` is owned and still alive here — only `&def.template` and `&def.allowed_tools` were borrowed earlier.)

- [ ] **Step 4: Run the test**

Run: `cargo test -p otto-engine --bin otto run_command`
Expected: PASS — both `run_command_expands_and_runs_spine` and `run_command_with_model_field_runs_offline`.

- [ ] **Step 5: Format, commit**

```bash
cargo fmt --all
git add crates/engine/src/main.rs
git commit -m "feat(engine): --command pins remote model from command frontmatter"
```

---

### Task 4: Wire `--agent` to the agent's `model`

The custom-agent path captures the target agent's `model` before the defs move into the registry, and pins the router handed to `TaskTool`.

**Files:**
- Modify: `crates/engine/src/main.rs` `run_custom_agent_in` (the `for def in ext.agents` loop ~line 374 and the `build_router()` at ~line 389)
- Test: `crates/engine/src/main.rs` `mod tests` (new test)

- [ ] **Step 1: Write the failing test**

Add to `crates/engine/src/main.rs`'s `mod tests`:

```rust
#[tokio::test]
async fn run_custom_agent_with_model_field_runs_offline() {
    use std::fs;
    // A custom agent declaring `model:` still runs a deterministic offline dispatch when no
    // ANTHROPIC_API_KEY is set (graceful fallback).
    let proj = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap(); // empty → no user-global agents
    let agents = proj.path().join(".claude").join("agents");
    fs::create_dir_all(&agents).unwrap();
    fs::write(
        agents.join("reviewer.md"),
        "---\nname: reviewer\ndescription: Reviews code\nmodel: claude-opus-4-8\n---\nYou review code.\n",
    )
    .unwrap();

    let ok = run_custom_agent_in(
        "reviewer",
        "look at lib.rs",
        proj.path().to_path_buf(),
        home.path().to_path_buf(),
    )
    .await;
    assert!(ok.is_ok(), "expected model-pinned agent to run offline: {ok:?}");
}
```

- [ ] **Step 2: Run the test (regression guard, expect PASS before and after)**

Run: `cargo test -p otto-engine --bin otto run_custom_agent_with_model_field`
Expected: PASS. As in Task 3, this is a regression guard — the offline outcome is identical with/without the Step 3 wiring; it locks in that a `model:` field does not break offline agent dispatch. Proceed to Step 3.

- [ ] **Step 3: Capture the target agent's model and pin the router**

In `crates/engine/src/main.rs`, inside `run_custom_agent_in`, add a capture variable before the registration loop and set it while iterating. The existing loop is:

```rust
    let mut registry = AgentRegistry::new();
    let mut allowlists: HashMap<String, Option<Vec<String>>> = HashMap::new();
    for def in ext.agents {
        allowlists.insert(def.name.clone(), def.tools.clone());
        registry.register(
            Role::Custom(def.name.clone()),
            Arc::new(MarkdownAgent::new(def)),
        );
    }
```

Replace it with (capture `model_override` for the named agent before `def` is moved):

```rust
    let mut registry = AgentRegistry::new();
    let mut allowlists: HashMap<String, Option<Vec<String>>> = HashMap::new();
    // Pin the remote model to the top-level `--agent`'s `model:` field. Nested Task
    // sub-dispatches inherit this same pinned router (per-sub-agent model is deferred).
    let mut model_override: Option<String> = None;
    for def in ext.agents {
        if def.name == name {
            model_override = def.model.clone();
        }
        allowlists.insert(def.name.clone(), def.tools.clone());
        registry.register(
            Role::Custom(def.name.clone()),
            Arc::new(MarkdownAgent::new(def)),
        );
    }
```

Then change the router construction (currently line 389):

```rust
    let router: Arc<dyn otto_engine_core::Router> = Arc::from(build_router());
```

to:

```rust
    let router: Arc<dyn otto_engine_core::Router> =
        Arc::from(otto_engine::build_router_with_model(model_override.as_deref()));
```

- [ ] **Step 4: Run the test**

Run: `cargo test -p otto-engine --bin otto run_custom_agent`
Expected: PASS.

- [ ] **Step 5: Format, lint, commit**

```bash
cargo fmt --all
cargo clippy -p otto-engine --all-targets
git add crates/engine/src/main.rs
git commit -m "feat(engine): --agent pins remote model from agent frontmatter"
```

---

### Task 5: Full-suite verification and docs

Confirms the whole workspace is green and records the slice.

**Files:**
- Modify: `CLAUDE.md` (extensions crate row — Slice 8 sentence; and the "model routing … remain the other open extensions threads" note)

- [ ] **Step 1: Run the full workspace suite**

Run: `cargo test --workspace`
Expected: PASS (all crates). This proves the offline determinism suite is untouched.

- [ ] **Step 2: Lint the whole workspace**

Run: `cargo clippy --workspace --all-targets`
Expected: no warnings.

- [ ] **Step 3: Update `CLAUDE.md`**

In the `extensions` crate row, append after the Slice 7 sentence:

```
Slice 8 adds per-artifact `model` routing for **commands and agents**: `run_command_in` /
`run_custom_agent_in` read the artifact's `model:` field and pass it to a new
`build_router_with_model(Option<&str>)`, which — when `ANTHROPIC_API_KEY` is present — builds
the remote slot as an `AnthropicProvider` pinned to that model id and wraps it in a
`PinnedModelRouter` (routes remote unless privacy-sensitive, which stays local; non-privacy
remote error falls back to local for liveness). With no key the named model can't be honored,
so it warns and falls back to the offline `SingleProviderRouter` — the deterministic default is
untouched. Only the top-level `--agent` model pins; nested `TaskTool` sub-dispatches inherit
that router. Skills' `model` stays inert (no invocation scope to pin); serve-path wiring and
per-sub-agent model remain deferred.
```

Update the trailing "open threads" note in the Slice 7 sentence from `model` routing being open to only serve-path wiring remaining.

In the `router` crate row, add `PinnedModelRouter` alongside `SingleProviderRouter`/`BrainBlendRouter`.

- [ ] **Step 4: Commit the docs**

```bash
git add CLAUDE.md
git commit -m "docs(extensions): record model routing (slice 8)"
```

- [ ] **Step 5: Final format check**

Run: `cargo fmt --all -- --check`
Expected: no diff.

---

## Spec coverage check

- Pin remote model per-turn → Tasks 2, 3, 4.
- `PinnedModelRouter` (remote-unless-privacy, non-privacy liveness fallback, no cross-boundary) → Task 1.
- `--command` + `--agent` scope → Tasks 3, 4.
- Privacy floor wins → Task 1 (`pinned_routes_local_for_privacy`, `pinned_privacy_error_never_crosses_to_remote`).
- Graceful fallback + warn → Task 2 (`build_router_with_model` no-key branch + test).
- Determinism preserved → Tasks 2, 3, 4 offline tests + Task 5 full suite.
- Docs → Task 5.
