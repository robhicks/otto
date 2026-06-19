# Token/Cost Meter + Pause/Resume Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a live token/cost meter and cooperative pause/resume for an in-flight turn (sub-project E), end to end from the providers through the orchestrator, serve transport, and the Leptos UI.

**Architecture:** Providers report token `Usage` on `CompleteResponse`; a `MeteringRouter` decorator tallies it into a shared `TokenMeter`; the orchestrator emits a cumulative `TokenCostMeter` event at each phase boundary (only when usage exists, so the offline stream is unchanged). Pause/Resume is a `PauseController` seam the orchestrator checks at phase boundaries, backed by a connection-scoped `AtomicBool`+`Notify` in serve and routed through the existing `split`+`select!` loop. The UI shows tokens (and a UI-derived `~$` cost) and a Pause/Resume button.

**Tech Stack:** Rust (workspace crates: `protocol`, `engine-core`, `providers`, `router`, `engine`), tokio, axum WebSocket, wiremock + tokio-tungstenite tests; Leptos CSR (Rust→WASM) for `ui/` (a standalone crate, not a workspace member).

**Design spec:** `docs/superpowers/specs/2026-06-19-ui-token-meter-pause-resume-design.md`

**Conventions:** Tests live next to code in `#[cfg(test)] mod tests`. Run `cargo fmt --all` before each commit. The workspace must build & test green after every task (`cargo build --workspace`, `cargo test --workspace`); the `ui/` crate is built/tested separately from inside `ui/`. Do not include any Claude self-attribution in commit messages.

---

## Task 1: Protocol — `Pause`/`Resume` commands + `TokenCostMeter` event

**Files:**
- Modify: `crates/protocol/src/lib.rs` (Command enum ~37-51, EventKind enum ~55-85, tests ~150+)
- Modify: `crates/engine/src/serve.rs` (outer command match ~323-395) — a temporary no-op arm so the workspace keeps compiling (the exhaustive `match command` would otherwise break). Real handling lands in Task 11.

- [ ] **Step 1: Write failing round-trip tests** in `crates/protocol/src/lib.rs`, inside `mod tests` (after `approve_diff_command_round_trips`):

```rust
    #[test]
    fn pause_and_resume_commands_round_trip() {
        let session = SessionId::new();
        for cmd in [
            Command::Pause { session },
            Command::Resume { session },
        ] {
            let back: Command =
                serde_json::from_str(&serde_json::to_string(&cmd).unwrap()).unwrap();
            assert_eq!(cmd, back);
        }
    }

    #[test]
    fn token_cost_meter_event_round_trips() {
        let event = Event {
            seq: 5,
            session: SessionId::new(),
            kind: EventKind::TokenCostMeter {
                input_tokens: 1234,
                output_tokens: 567,
            },
        };
        let back: Event = serde_json::from_str(&serde_json::to_string(&event).unwrap()).unwrap();
        assert_eq!(event, back);
    }
```

- [ ] **Step 2: Run the tests to verify they fail to compile**

Run: `cargo test -p otto-protocol pause_and_resume_commands_round_trip token_cost_meter_event_round_trips`
Expected: FAIL — `no variant named Pause` / `no variant named TokenCostMeter`.

- [ ] **Step 3: Add the variants.** In `crates/protocol/src/lib.rs`, add to `enum Command` (after the `ApproveDiff { … }` variant, before the closing `}`):

```rust
    Pause {
        session: SessionId,
    },
    Resume {
        session: SessionId,
    },
```

In `enum EventKind`, add (after the `TurnComplete { ok: bool }` variant):

```rust
    /// Cumulative token usage for the current turn, emitted as the turn progresses. Only fires
    /// when a metered provider reported usage (the offline path emits none). The UI renders the
    /// counts and derives an approximate cost from the remote model in the capabilities manifest.
    TokenCostMeter {
        input_tokens: u64,
        output_tokens: u64,
    },
```

- [ ] **Step 4: Keep the workspace compiling — add a no-op arm to serve's outer match.** In `crates/engine/src/serve.rs`, the outer `match command { … }` (~line 323) is exhaustive. After the `Command::CreateSession => { … }` arm, add:

```rust
            Command::Pause { .. } | Command::Resume { .. } => {
                // No turn in flight: pause/resume have no effect here. Real connection-scoped
                // handling is wired in the serve pause/resume task.
            }
```

(The inner `select!` `match` already has a `_ => {}` arm, so it needs no change yet.)

- [ ] **Step 5: Run tests + workspace build**

Run: `cargo test -p otto-protocol && cargo build --workspace`
Expected: PASS; workspace builds.

- [ ] **Step 6: Commit**

```bash
cargo fmt --all
git add crates/protocol/src/lib.rs crates/engine/src/serve.rs
git commit -m "feat(protocol): Pause/Resume commands + TokenCostMeter event"
```

---

## Task 2: engine-core — `Usage` type + `CompleteResponse.usage`; provider constructors; `ScriptedProvider::with_usage`

**Files:**
- Modify: `crates/engine-core/src/types.rs` (CompleteResponse ~13-17; export)
- Modify: `crates/engine-core/src/lib.rs` (types re-export ~18)
- Modify: `crates/providers/src/local.rs:31`, `crates/providers/src/scripted.rs` (struct + builder + ctor + complete), `crates/providers/src/ollama.rs:65`, `crates/providers/src/anthropic.rs:97`
- Modify: `crates/engine-core/src/orchestrator.rs:279` (FakeRouter test), `crates/router/src/lib.rs:139,152` (test providers)

This task adds the `usage` field and makes everything compile with `usage: None`. Real parsing for Anthropic/Ollama lands in Tasks 3–4.

- [ ] **Step 1: Add the `Usage` type and field.** In `crates/engine-core/src/types.rs`, replace the `CompleteResponse` struct (lines 13-17) with:

```rust
/// Token usage reported by a provider for one completion. Absent for providers that do not
/// report it (the offline `LocalProvider`/`ScriptedProvider`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: u32,
    pub output_tokens: u32,
}

/// A provider's completion.
#[derive(Debug, Clone, PartialEq)]
pub struct CompleteResponse {
    pub text: String,
    /// Token usage, when the provider reports it. `None` on the offline/deterministic path.
    pub usage: Option<Usage>,
}
```

- [ ] **Step 2: Export `Usage`.** In `crates/engine-core/src/lib.rs`, add `Usage` to the types re-export (line 18):

```rust
pub use types::{AgentOutput, AgentRequest, CompleteRequest, CompleteResponse, Edit, Milestone, Usage};
```

- [ ] **Step 3: Run the build to find every broken constructor**

Run: `cargo build --workspace`
Expected: FAIL — `missing field 'usage'` at the 6 constructor sites below.

- [ ] **Step 4: Fix the offline/test constructors to `usage: None`.**

`crates/providers/src/local.rs` (line ~31):

```rust
        Ok(CompleteResponse {
            text: format!("// generated by otto local provider\n{}\n", req.prompt),
            usage: None,
        })
```

`crates/engine-core/src/orchestrator.rs` (FakeRouter, line ~279):

```rust
            Ok(CompleteResponse {
                text: "fake".to_string(),
                usage: None,
            })
```

`crates/router/src/lib.rs` EchoProvider (line ~139):

```rust
            Ok(CompleteResponse {
                text: format!("echo:{}", req.prompt),
                usage: None,
            })
```

`crates/router/src/lib.rs` TagProvider (line ~152):

```rust
            Ok(CompleteResponse {
                text: self.0.to_string(),
                usage: None,
            })
```

(Anthropic line ~97 and Ollama line ~65 use shorthand `CompleteResponse { text }` — change them to `CompleteResponse { text, usage: None }` for now; Tasks 3–4 replace them with parsed usage.)

`crates/providers/src/anthropic.rs` (line ~97): `Ok(CompleteResponse { text, usage: None })`
`crates/providers/src/ollama.rs` (line ~65):

```rust
        Ok(CompleteResponse {
            text: resp.response,
            usage: None,
        })
```

- [ ] **Step 5: Add `usage` support to `ScriptedProvider`** (so later tests can opt into metered responses). In `crates/providers/src/scripted.rs`:

Add the import (line ~7): `use otto_engine_core::types::{CompleteRequest, CompleteResponse, Usage};`

Replace the struct + `new` + add a builder:

```rust
/// Returns the first rule whose `needle` is found in the prompt, else `default`.
pub struct ScriptedProvider {
    rules: Vec<(String, String)>,
    default: String,
    usage: Option<Usage>,
}

impl ScriptedProvider {
    pub fn new(default: impl Into<String>) -> Self {
        Self {
            rules: Vec::new(),
            default: default.into(),
            usage: None,
        }
    }

    /// Add a rule: if the prompt contains `needle`, return `response`. First match wins.
    pub fn on(mut self, needle: impl Into<String>, response: impl Into<String>) -> Self {
        self.rules.push((needle.into(), response.into()));
        self
    }

    /// Make every response report this token usage (for metering tests).
    pub fn with_usage(mut self, input_tokens: u32, output_tokens: u32) -> Self {
        self.usage = Some(Usage {
            input_tokens,
            output_tokens,
        });
        self
    }
}
```

Replace the `complete` body's return:

```rust
        Ok(CompleteResponse {
            text,
            usage: self.usage,
        })
```

- [ ] **Step 6: Write a test for `with_usage`** in `crates/providers/src/scripted.rs` `mod tests`:

```rust
    #[tokio::test]
    async fn with_usage_propagates_to_responses() {
        let p = ScriptedProvider::new("X").with_usage(7, 11);
        let out = p
            .complete(CompleteRequest {
                prompt: "anything".into(),
            })
            .await
            .unwrap();
        assert_eq!(out.usage, Some(otto_engine_core::types::Usage { input_tokens: 7, output_tokens: 11 }));
    }
```

- [ ] **Step 7: Run the build + tests**

Run: `cargo build --workspace && cargo test -p otto-providers && cargo test -p otto-engine-core && cargo test -p otto-router`
Expected: PASS (existing tests still green; `with_usage_propagates_to_responses` passes).

- [ ] **Step 8: Commit**

```bash
cargo fmt --all
git add crates/engine-core/src/types.rs crates/engine-core/src/lib.rs crates/engine-core/src/orchestrator.rs crates/providers/src crates/router/src/lib.rs
git commit -m "feat(engine-core): Usage on CompleteResponse; ScriptedProvider::with_usage"
```

---

## Task 3: providers — Anthropic parses `usage`

**Files:**
- Modify: `crates/providers/src/anthropic.rs` (MessagesResponse ~59-62, complete ~91-97, tests)

- [ ] **Step 1: Write a failing test** in `crates/providers/src/anthropic.rs` `mod tests`:

```rust
    #[tokio::test]
    async fn anthropic_parses_usage_tokens() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "content": [{ "type": "text", "text": "hi" }],
                "usage": { "input_tokens": 12, "output_tokens": 34 }
            })))
            .mount(&server)
            .await;
        let provider = AnthropicProvider::new(server.uri(), "k", "claude-haiku-4-5");
        let out = provider
            .complete(CompleteRequest { prompt: "hi".into() })
            .await
            .unwrap();
        assert_eq!(
            out.usage,
            Some(otto_engine_core::types::Usage { input_tokens: 12, output_tokens: 34 })
        );
    }
```

Add `use otto_engine_core::types::CompleteRequest;` to the test module if not already resolvable (it is via `super::*`; `CompleteRequest` is re-exported through the top-of-file `use`). No new import needed.

- [ ] **Step 2: Run it to verify failure**

Run: `cargo test -p otto-providers anthropic_parses_usage_tokens`
Expected: FAIL — `out.usage` is `None`.

- [ ] **Step 3: Parse usage.** In `crates/providers/src/anthropic.rs`, extend the response structs (after `MessagesResponse`):

```rust
#[derive(Deserialize)]
struct ApiUsage {
    #[serde(default)]
    input_tokens: u32,
    #[serde(default)]
    output_tokens: u32,
}

#[derive(Deserialize)]
struct MessagesResponse {
    content: Vec<ContentBlock>,
    #[serde(default)]
    usage: Option<ApiUsage>,
}
```

(Delete the old `MessagesResponse` definition — keep only the one with `usage`.) Then in `complete`, build the response (replace the final `let text = …; Ok(CompleteResponse { text, usage: None })` block):

```rust
        let usage = resp.usage.as_ref().map(|u| otto_engine_core::types::Usage {
            input_tokens: u.input_tokens,
            output_tokens: u.output_tokens,
        });
        let text = resp
            .content
            .into_iter()
            .map(|b| b.text)
            .collect::<Vec<_>>()
            .join("");
        Ok(CompleteResponse { text, usage })
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p otto-providers anthropic`
Expected: PASS (new test + the two existing anthropic tests; the existing ones omit `usage` → `None`, still fine).

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
git add crates/providers/src/anthropic.rs
git commit -m "feat(providers): Anthropic reports token usage"
```

---

## Task 4: providers — Ollama parses token counts

**Files:**
- Modify: `crates/providers/src/ollama.rs` (GenerateResponse ~38-41, complete ~64-68, tests)

- [ ] **Step 1: Write a failing test** in `crates/providers/src/ollama.rs` `mod tests`:

```rust
    #[tokio::test]
    async fn ollama_parses_eval_counts_as_usage() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/generate"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "response": "hi",
                "prompt_eval_count": 9,
                "eval_count": 5,
                "done": true
            })))
            .mount(&server)
            .await;
        let provider = OllamaProvider::new(server.uri(), "llama3.2");
        let out = provider
            .complete(CompleteRequest { prompt: "hi".into() })
            .await
            .unwrap();
        assert_eq!(
            out.usage,
            Some(otto_engine_core::types::Usage { input_tokens: 9, output_tokens: 5 })
        );
    }
```

- [ ] **Step 2: Run it to verify failure**

Run: `cargo test -p otto-providers ollama_parses_eval_counts_as_usage`
Expected: FAIL — `out.usage` is `None`.

- [ ] **Step 3: Parse the counts.** In `crates/providers/src/ollama.rs`, extend `GenerateResponse`:

```rust
#[derive(Deserialize)]
struct GenerateResponse {
    response: String,
    #[serde(default)]
    prompt_eval_count: u32,
    #[serde(default)]
    eval_count: u32,
}
```

Replace the `complete` return:

```rust
        Ok(CompleteResponse {
            text: resp.response,
            usage: Some(otto_engine_core::types::Usage {
                input_tokens: resp.prompt_eval_count,
                output_tokens: resp.eval_count,
            }),
        })
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p otto-providers ollama`
Expected: PASS. (The existing `ollama_posts_generate_and_parses_response` test's mock omits the counts → they default to 0, so `usage` is `Some(Usage{0,0})`; that test only asserts `out.text`, so it stays green.)

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
git add crates/providers/src/ollama.rs
git commit -m "feat(providers): Ollama reports token usage"
```

---

## Task 5: engine-core — `TokenMeter` accumulator

**Files:**
- Create: `crates/engine-core/src/meter.rs`
- Modify: `crates/engine-core/src/lib.rs` (add `pub mod meter;` + re-export)

- [ ] **Step 1: Write the module with a failing test.** Create `crates/engine-core/src/meter.rs`:

```rust
//! A cheap, shareable accumulator of token usage for one turn. The `MeteringRouter`
//! (in the `router` crate) writes to it as completions pass through; the orchestrator reads
//! the running totals to emit `TokenCostMeter` events.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::types::Usage;

/// Cumulative input/output token counters. `Default` starts at zero.
#[derive(Default)]
pub struct TokenMeter {
    input: AtomicU64,
    output: AtomicU64,
}

impl TokenMeter {
    /// Add one completion's usage to the running totals.
    pub fn add(&self, u: &Usage) {
        self.input.fetch_add(u.input_tokens as u64, Ordering::SeqCst);
        self.output.fetch_add(u.output_tokens as u64, Ordering::SeqCst);
    }

    /// `(input_tokens, output_tokens)` accumulated so far.
    pub fn snapshot(&self) -> (u64, u64) {
        (
            self.input.load(Ordering::SeqCst),
            self.output.load(Ordering::SeqCst),
        )
    }

    /// Total tokens (input + output). Used to gate emission: zero means "no usage yet".
    pub fn total(&self) -> u64 {
        let (i, o) = self.snapshot();
        i + o
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accumulates_and_snapshots() {
        let m = TokenMeter::default();
        assert_eq!(m.snapshot(), (0, 0));
        assert_eq!(m.total(), 0);
        m.add(&Usage { input_tokens: 2, output_tokens: 3 });
        m.add(&Usage { input_tokens: 1, output_tokens: 1 });
        assert_eq!(m.snapshot(), (3, 4));
        assert_eq!(m.total(), 7);
    }
}
```

- [ ] **Step 2: Wire the module + export.** In `crates/engine-core/src/lib.rs`, add `pub mod meter;` (after `pub mod types;`) and a re-export line:

```rust
pub use meter::TokenMeter;
```

- [ ] **Step 3: Run the test**

Run: `cargo test -p otto-engine-core meter::`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
cargo fmt --all
git add crates/engine-core/src/meter.rs crates/engine-core/src/lib.rs
git commit -m "feat(engine-core): TokenMeter usage accumulator"
```

---

## Task 6: router — `MeteringRouter` decorator

**Files:**
- Modify: `crates/router/src/lib.rs` (add struct + impl + tests; it is auto-public)

- [ ] **Step 1: Write a failing test** in `crates/router/src/lib.rs` `mod tests` (after the existing providers; `TokenMeter`/`Usage` come from `otto_engine_core`):

```rust
    use otto_engine_core::types::Usage;
    use otto_engine_core::TokenMeter;

    struct UsageRouter(Option<Usage>);
    #[async_trait]
    impl Router for UsageRouter {
        async fn complete(
            &self,
            _req: CompleteRequest,
            _hints: RouteHints,
        ) -> anyhow::Result<CompleteResponse> {
            Ok(CompleteResponse {
                text: "x".to_string(),
                usage: self.0,
            })
        }
    }

    #[tokio::test]
    async fn metering_router_tallies_usage_and_passes_through() {
        let meter = Arc::new(TokenMeter::default());
        let inner: Arc<dyn Router> = Arc::new(UsageRouter(Some(Usage {
            input_tokens: 2,
            output_tokens: 3,
        })));
        let r = MeteringRouter::new(inner, Arc::clone(&meter));

        let out = r
            .complete(CompleteRequest { prompt: "p".into() }, RouteHints::default())
            .await
            .unwrap();
        assert_eq!(out.text, "x"); // passed through unchanged
        assert_eq!(meter.snapshot(), (2, 3));

        r.complete(CompleteRequest { prompt: "p".into() }, RouteHints::default())
            .await
            .unwrap();
        assert_eq!(meter.snapshot(), (4, 6)); // cumulative
    }

    #[tokio::test]
    async fn metering_router_ignores_none_usage() {
        let meter = Arc::new(TokenMeter::default());
        let inner: Arc<dyn Router> = Arc::new(UsageRouter(None));
        let r = MeteringRouter::new(inner, Arc::clone(&meter));
        r.complete(CompleteRequest { prompt: "p".into() }, RouteHints::default())
            .await
            .unwrap();
        assert_eq!(meter.snapshot(), (0, 0));
    }
```

- [ ] **Step 2: Run it to verify failure**

Run: `cargo test -p otto-router metering_router`
Expected: FAIL — `MeteringRouter` not found.

- [ ] **Step 3: Implement the decorator.** In `crates/router/src/lib.rs`, add the import for `TokenMeter` at the top (with the other `otto_engine_core` uses, ~line 9-11):

```rust
use otto_engine_core::TokenMeter;
```

Then add (e.g. after `BrainBlendRouter`'s impl, before `#[cfg(test)]`):

```rust
/// A `Router` decorator that tallies each completion's reported token usage into a shared
/// `TokenMeter`, passing the response through unchanged. Agents are unaffected — they still
/// receive a `CompleteResponse`. Per-turn, the engine wraps the real router in this and reads
/// the meter to emit `TokenCostMeter` events.
pub struct MeteringRouter {
    inner: Arc<dyn Router>,
    meter: Arc<TokenMeter>,
}

impl MeteringRouter {
    pub fn new(inner: Arc<dyn Router>, meter: Arc<TokenMeter>) -> Self {
        Self { inner, meter }
    }
}

#[async_trait]
impl Router for MeteringRouter {
    async fn complete(
        &self,
        req: CompleteRequest,
        hints: RouteHints,
    ) -> anyhow::Result<CompleteResponse> {
        let resp = self.inner.complete(req, hints).await?;
        if let Some(u) = &resp.usage {
            self.meter.add(u);
        }
        Ok(resp)
    }
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p otto-router`
Expected: PASS (both new tests + all existing router tests).

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
git add crates/router/src/lib.rs
git commit -m "feat(router): MeteringRouter tallies token usage"
```

---

## Task 7: engine-core — `PauseController` seam + `NeverPause`

**Files:**
- Modify: `crates/engine-core/src/tool.rs` (add trait + default + test, beside `Approver`)
- Modify: `crates/engine-core/src/lib.rs` (tool re-export ~13-16)

- [ ] **Step 1: Write a failing test** in `crates/engine-core/src/tool.rs` `mod tests`:

```rust
    #[tokio::test]
    async fn never_pause_does_not_pause() {
        let p = NeverPause;
        assert!(!p.should_pause());
        p.wait_for_resume().await; // returns immediately
    }
```

- [ ] **Step 2: Run it to verify failure**

Run: `cargo test -p otto-engine-core never_pause_does_not_pause`
Expected: FAIL — `NeverPause` not found.

- [ ] **Step 3: Add the seam.** In `crates/engine-core/src/tool.rs`, after `DenyApprover`'s impl (~line 67), add:

```rust
/// Cooperative pause for an in-flight turn. The orchestrator calls this at each phase
/// boundary: if a pause is requested it parks the turn until resumed. The default never
/// pauses, so CLI/headless/offline runs are unaffected.
#[async_trait]
pub trait PauseController: Send + Sync {
    /// A sync peek at a phase boundary: is a pause currently requested?
    fn should_pause(&self) -> bool;
    /// Park until resumed (or released on disconnect/abort). Returns promptly if not paused.
    async fn wait_for_resume(&self);
}

/// Default: never pauses.
pub struct NeverPause;

#[async_trait]
impl PauseController for NeverPause {
    fn should_pause(&self) -> bool {
        false
    }
    async fn wait_for_resume(&self) {}
}
```

- [ ] **Step 4: Export the seam.** In `crates/engine-core/src/lib.rs`, add `NeverPause, PauseController` to the `tool::{…}` re-export:

```rust
pub use tool::{
    AllowListAskResolver, Approver, AskResolver, Decision, DenyApprover, DenyAsk, NeverPause,
    PauseController, PermissionGate, Tool, ToolRegistry,
};
```

- [ ] **Step 5: Run the test**

Run: `cargo test -p otto-engine-core never_pause_does_not_pause && cargo build --workspace`
Expected: PASS; workspace builds.

- [ ] **Step 6: Commit**

```bash
cargo fmt --all
git add crates/engine-core/src/tool.rs crates/engine-core/src/lib.rs
git commit -m "feat(engine-core): PauseController seam + NeverPause default"
```

---

## Task 8: engine-core — Orchestrator emits `TokenCostMeter` (+ add `meter` & `pauser` fields)

This task adds **both** new `Orchestrator` fields (`meter`, `pauser`) and updates every test construction once, then implements meter emission. The `pauser` field is added now (a `pub` field — no dead-code warning) and wired in Task 9.

**Files:**
- Modify: `crates/engine-core/src/orchestrator.rs` (struct ~31-41, run_turn ~47-214, test imports ~220-229, 8 test constructions, new tests)

- [ ] **Step 1: Add the two fields to the struct.** In `crates/engine-core/src/orchestrator.rs`, extend `pub struct Orchestrator<'a>` (after `next_id`):

```rust
    /// Running token totals for this turn (fed by the engine's `MeteringRouter`). The
    /// orchestrator reads it to emit `TokenCostMeter`; zero totals (offline) emit nothing.
    pub meter: &'a crate::meter::TokenMeter,
    /// Cooperative pause checked at phase boundaries (wired in the pause task).
    pub pauser: &'a dyn crate::tool::PauseController,
```

- [ ] **Step 2: Add the emission helper + calls.** Add a method on `impl<'a> Orchestrator<'a>` (e.g. just before `run_turn`):

```rust
    /// Emit a cumulative meter event — but only when usage has been recorded, so the offline
    /// path (no usage) emits nothing and its event stream is unchanged.
    fn emit_meter(&self, emit: &dyn Emitter) {
        if self.meter.total() > 0 {
            let (input_tokens, output_tokens) = self.meter.snapshot();
            emit.emit(EventKind::TokenCostMeter {
                input_tokens,
                output_tokens,
            });
        }
    }
```

In `run_turn`, call `self.emit_meter(emit);` immediately **after each** `AgentFinished` emit — there are four:
- after `emit.emit(EventKind::AgentFinished { role: Role::Planner });` (~line 77)
- after `emit.emit(EventKind::AgentFinished { role: Role::ContextFinder });` (~line 98)
- after `emit.emit(EventKind::AgentFinished { role: Role::Coder });` (~line 176)
- after `emit.emit(EventKind::AgentFinished { role: Role::Verifier });` (~line 196)

- [ ] **Step 3: Update test imports.** In the `mod tests` `use` block (~lines 221-229), change:

```rust
    use crate::meter::TokenMeter;
    use crate::tool::{
        Approver, Decision, DenyApprover, DenyAsk, NeverPause, PermissionGate, ToolRegistry,
    };
    use crate::types::{CompleteRequest, CompleteResponse, Edit, Milestone, Usage, WorkspaceSnapshot};
```

(Add `use crate::meter::TokenMeter;`, add `NeverPause` to the tool import, add `Usage` to the types import. Keep the other existing imports.)

- [ ] **Step 4: Update every `Orchestrator { … }` literal in tests.** There are **8** literals (in `run_turn_drives_full_spine_and_emits_ordered_events`, `run_turn_errors_when_a_role_is_missing`, `denied_edit_is_skipped_and_logged`, `ask_verdict_also_skips_edit_fail_closed`, `flaky_verifier_triggers_repair_then_succeeds`, `repair_exhaustion_fails_the_turn`, `ask_edit_approved_is_applied_and_emits_request`, `ask_edit_rejected_is_skipped_but_turn_completes`). In each test, add a `let meter = TokenMeter::default();` near the other `let`s, and add these two fields to the literal:

```rust
            meter: &meter,
            pauser: &NeverPause,
```

After editing, run `cargo build -p otto-engine-core --tests`; any `missing field 'meter'`/`'pauser'` error points to a literal you missed — add the two fields there.

- [ ] **Step 5: Write the new meter tests** in `mod tests`:

```rust
    #[tokio::test]
    async fn emits_cumulative_token_cost_meter_when_usage_present() {
        let reg = registry();
        let router = FakeRouter;
        let workspace = RecordingWorkspace::default();
        let tools = empty_tools();
        let meter = TokenMeter::default();
        // Simulate usage recorded by the MeteringRouter during the turn.
        meter.add(&Usage { input_tokens: 3, output_tokens: 5 });
        let orch = Orchestrator {
            registry: &reg,
            router: &router,
            workspace: &workspace,
            tools: &tools,
            approver: &DenyApprover,
            next_id: &test_id,
            meter: &meter,
            pauser: &NeverPause,
        };
        let events: Arc<Mutex<Vec<EventKind>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = {
            let events = Arc::clone(&events);
            move |kind: EventKind| events.lock().unwrap().push(kind)
        };
        orch.run_turn(SessionId::new(), "x", &sink).await.unwrap();
        let recorded = events.lock().unwrap().clone();
        assert!(recorded.iter().any(|e| matches!(
            e,
            EventKind::TokenCostMeter { input_tokens: 3, output_tokens: 5 }
        )));
    }

    #[tokio::test]
    async fn no_token_cost_meter_when_usage_absent() {
        let reg = registry();
        let router = FakeRouter;
        let workspace = RecordingWorkspace::default();
        let tools = empty_tools();
        let meter = TokenMeter::default(); // stays zero — offline path
        let orch = Orchestrator {
            registry: &reg,
            router: &router,
            workspace: &workspace,
            tools: &tools,
            approver: &DenyApprover,
            next_id: &test_id,
            meter: &meter,
            pauser: &NeverPause,
        };
        let events: Arc<Mutex<Vec<EventKind>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = {
            let events = Arc::clone(&events);
            move |kind: EventKind| events.lock().unwrap().push(kind)
        };
        orch.run_turn(SessionId::new(), "x", &sink).await.unwrap();
        let recorded = events.lock().unwrap().clone();
        assert!(
            !recorded
                .iter()
                .any(|e| matches!(e, EventKind::TokenCostMeter { .. })),
            "offline path (no usage) must emit no meter events"
        );
    }
```

- [ ] **Step 6: Run the tests**

Run: `cargo test -p otto-engine-core orchestrator::`
Expected: PASS. The exact-sequence test `run_turn_drives_full_spine_and_emits_ordered_events` still passes (its meter is zero → no `TokenCostMeter` inserted).

- [ ] **Step 7: Commit**

```bash
cargo fmt --all
git add crates/engine-core/src/orchestrator.rs
git commit -m "feat(engine-core): orchestrator emits cumulative TokenCostMeter"
```

---

## Task 9: engine-core — Orchestrator pause checkpoint at phase boundaries

**Files:**
- Modify: `crates/engine-core/src/orchestrator.rs` (run_turn checkpoints, new test; uses the `pauser` field from Task 8)

- [ ] **Step 1: Add the checkpoint helper + calls.** In `impl<'a> Orchestrator<'a>`, add:

```rust
    /// At a phase boundary: if a pause is requested, park the turn until resumed, bracketing
    /// the park with `Log` lines so the pause is recorded in the event stream.
    async fn checkpoint(&self, emit: &dyn Emitter) {
        if self.pauser.should_pause() {
            emit.emit(EventKind::Log {
                message: "turn paused".to_string(),
            });
            self.pauser.wait_for_resume().await;
            emit.emit(EventKind::Log {
                message: "turn resumed".to_string(),
            });
        }
    }
```

In `run_turn`, call `self.checkpoint(emit).await;` at three phase boundaries:
- at the very start of `run_turn`, before `emit.emit(EventKind::AgentStarted { role: Role::Planner });` (~line 56)
- before `emit.emit(EventKind::AgentStarted { role: Role::ContextFinder });` (~line 80)
- at the **top of the `let ok = loop {` body**, before `emit.emit(EventKind::AgentStarted { role: Role::Coder });` (~line 109) — so each repair iteration checkpoints too.

- [ ] **Step 2: Write a failing test** in `mod tests`:

```rust
    struct PauseOnce {
        fired: AtomicBool,
    }
    #[async_trait]
    impl crate::tool::PauseController for PauseOnce {
        fn should_pause(&self) -> bool {
            // Pause on the first checkpoint only, then run freely.
            !self.fired.swap(true, Ordering::SeqCst)
        }
        async fn wait_for_resume(&self) {}
    }

    #[tokio::test]
    async fn pause_checkpoint_brackets_with_logs_and_completes() {
        let reg = registry();
        let router = FakeRouter;
        let workspace = RecordingWorkspace::default();
        let tools = empty_tools();
        let meter = TokenMeter::default();
        let pauser = PauseOnce {
            fired: AtomicBool::new(false),
        };
        let orch = Orchestrator {
            registry: &reg,
            router: &router,
            workspace: &workspace,
            tools: &tools,
            approver: &DenyApprover,
            next_id: &test_id,
            meter: &meter,
            pauser: &pauser,
        };
        let events: Arc<Mutex<Vec<EventKind>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = {
            let events = Arc::clone(&events);
            move |kind: EventKind| events.lock().unwrap().push(kind)
        };
        let outcome = orch.run_turn(SessionId::new(), "x", &sink).await.unwrap();
        assert_eq!(outcome, TurnOutcome { ok: true });
        let recorded = events.lock().unwrap().clone();
        assert!(recorded.iter().any(|e| matches!(e, EventKind::Log { message } if message == "turn paused")));
        assert!(recorded.iter().any(|e| matches!(e, EventKind::Log { message } if message == "turn resumed")));
    }
```

Add `AtomicBool` to the atomics import in `mod tests` (~line 227): `use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};`.

- [ ] **Step 3: Run the test**

Run: `cargo test -p otto-engine-core pause_checkpoint_brackets_with_logs_and_completes`
Expected: PASS. (And `cargo test -p otto-engine-core orchestrator::` — the existing exact-sequence test still passes: `NeverPause::should_pause()` is false, so no checkpoint logs are inserted.)

- [ ] **Step 4: Commit**

```bash
cargo fmt --all
git add crates/engine-core/src/orchestrator.rs
git commit -m "feat(engine-core): cooperative pause checkpoint at phase boundaries"
```

---

## Task 10: engine — `TurnControls` + `run_prompt_with_controls` + meter/pauser wiring

Wraps the shared router in `MeteringRouter` and a per-turn `TokenMeter`, passes both new handles to the orchestrator, and bundles approver+pauser into `TurnControls`. `run_prompt_with_approver` stays as a delegating wrapper so serve keeps compiling (migrated in Task 11). After this task, the meter already flows end-to-end through serve.

**Files:**
- Modify: `crates/engine/src/service.rs` (imports ~6-16, struct/methods ~89-194; export via `crates/engine/src/lib.rs` ~36)

- [ ] **Step 1: Update imports.** In `crates/engine/src/service.rs`, change the engine-core/tool imports near the top:

```rust
use otto_engine_core::tool::{Approver, Decision, DenyApprover, NeverPause, PauseController, ToolRegistry};
use otto_engine_core::traits::Workspace;
use otto_engine_core::types::Edit;
use otto_engine_core::{AgentRegistry, Orchestrator, Router, TokenMeter, TurnOutcome};
use otto_router::MeteringRouter;
```

- [ ] **Step 2: Add `TurnControls`.** In `crates/engine/src/service.rs`, after the `EventSink`/`CollectingSink` definitions (~line 37), add:

```rust
/// The per-turn control handles the serve layer can inject: how `Ask` edits are approved, and
/// how the turn is paused. `Default` is the headless/CLI posture (deny approvals, never pause).
pub struct TurnControls {
    pub approver: Arc<dyn Approver>,
    pub pauser: Arc<dyn PauseController>,
}

impl Default for TurnControls {
    fn default() -> Self {
        Self {
            approver: Arc::new(DenyApprover),
            pauser: Arc::new(NeverPause),
        }
    }
}
```

- [ ] **Step 3: Repoint `run_prompt` + `run_prompt_with_approver` at a new `run_prompt_with_controls`.** Replace the two methods (`run_prompt` ~89-97 and `run_prompt_with_approver` ~106-194) so that:

`run_prompt` delegates with defaults:

```rust
    /// Run one turn with the headless defaults (deny approvals, never pause). (≙ `SendPrompt`.)
    pub async fn run_prompt(
        &self,
        session: SessionId,
        goal: &str,
        sink: &mut dyn EventSink,
    ) -> anyhow::Result<TurnOutcome> {
        self.run_prompt_with_controls(session, goal, sink, TurnControls::default())
            .await
    }

    /// Back-compat wrapper: run with `approver` and no pause. (Used by serve until it migrates
    /// to `run_prompt_with_controls`.)
    pub async fn run_prompt_with_approver(
        &self,
        session: SessionId,
        goal: &str,
        sink: &mut dyn EventSink,
        approver: Arc<dyn Approver>,
    ) -> anyhow::Result<TurnOutcome> {
        self.run_prompt_with_controls(
            session,
            goal,
            sink,
            TurnControls {
                approver,
                pauser: Arc::new(NeverPause),
            },
        )
        .await
    }
```

Rename the full method to `run_prompt_with_controls` taking `controls: TurnControls`, and change the spawned turn block to build the meter + metering router and pass the new handles. The full method body:

```rust
    /// Run one orchestrator turn for `goal`, streaming each event to `sink` after persisting it
    /// (fail-closed), recording the turn, and updating status. `controls` supply the approver
    /// and pause controller. The seq sequence continues from the store. One turn at a time.
    pub async fn run_prompt_with_controls(
        &self,
        session: SessionId,
        goal: &str,
        sink: &mut dyn EventSink,
        controls: TurnControls,
    ) -> anyhow::Result<TurnOutcome> {
        let _guard = self.turn_lock.lock().await;

        let start_seq = self.store.next_seq(session).await?;
        let turn_index = self.store.next_turn(session).await?;

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Event>();

        let handle = {
            let registry = Arc::clone(&self.registry);
            let router = Arc::clone(&self.router);
            let workspace = Arc::clone(&self.workspace);
            let tools = Arc::clone(&self.tools);
            let goal = goal.to_string();
            let counter = Arc::new(AtomicU64::new(start_seq));
            let approver = Arc::clone(&controls.approver);
            let pauser = Arc::clone(&controls.pauser);
            tokio::spawn(async move {
                // Per-turn meter; the metering router tallies usage as completions pass through.
                let meter = Arc::new(TokenMeter::default());
                let metering_router = MeteringRouter::new(router, Arc::clone(&meter));
                let sink_fn = move |kind: EventKind| {
                    let seq = counter.fetch_add(1, Ordering::SeqCst);
                    let _ = tx.send(Event { seq, session, kind });
                };
                let next_id = || uuid::Uuid::new_v4();
                let orchestrator = Orchestrator {
                    registry: &registry,
                    router: &metering_router,
                    workspace: &*workspace,
                    tools: &tools,
                    approver: &*approver,
                    next_id: &next_id,
                    meter: &meter,
                    pauser: &*pauser,
                };
                orchestrator.run_turn(session, &goal, &sink_fn).await
            })
        };

        let mut stream_err: Option<anyhow::Error> = None;
        while let Some(event) = rx.recv().await {
            if let Err(e) = self.store.append_event(session, &event).await {
                stream_err = Some(e);
                break;
            }
            if let Err(e) = sink.emit(&event).await {
                stream_err = Some(e);
                break;
            }
        }
        drop(rx);

        let turn_result = handle.await?;

        if let Some(e) = stream_err {
            let _ = self.store.set_status(session, SessionStatus::Failed).await;
            return Err(e);
        }
        let outcome = match turn_result {
            Ok(outcome) => outcome,
            Err(e) => {
                let _ = self.store.set_status(session, SessionStatus::Failed).await;
                return Err(e);
            }
        };

        self.store
            .record_turn(
                session,
                &TurnRecord {
                    turn_index,
                    goal: goal.to_string(),
                    outcome: serde_json::json!({ "ok": outcome.ok }),
                },
            )
            .await?;
        let status = if outcome.ok {
            SessionStatus::Done
        } else {
            SessionStatus::Failed
        };
        self.store.set_status(session, status).await?;

        Ok(outcome)
    }
```

- [ ] **Step 4: Export `TurnControls`.** In `crates/engine/src/lib.rs`, extend the service re-export (~line 36):

```rust
pub use service::{CollectingSink, EngineService, EventSink, TurnControls};
```

- [ ] **Step 5: Add a service test that the meter streams.** In `crates/engine/src/service.rs` `mod tests`, add a router builder + test:

```rust
    fn metered_router() -> Arc<dyn Router> {
        let provider = ScriptedProvider::new("{}")
            .on("edits", r#"{"edits": [{"path": "out.txt", "contents": "hi g"}]}"#)
            .on("milestones", r#"{"milestones": [{"description": "x"}]}"#)
            .with_usage(10, 20);
        Arc::new(SingleProviderRouter::new(Arc::new(provider)))
    }

    #[tokio::test]
    async fn run_prompt_streams_token_cost_meter_with_usage() {
        let dir = tempfile::tempdir().unwrap();
        // Build a service whose router reports usage.
        let store: Arc<dyn SessionStore> =
            Arc::new(SqliteStore::open(dir.path().join("s.db")).await.unwrap());
        let workspace: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(dir.path()));
        let tools_ws: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(dir.path()));
        let tools = Arc::new(crate::build_tool_registry(tools_ws, dir.path().to_path_buf()));
        let service = EngineService::new(
            store,
            Arc::new(crate::build_default_registry()),
            metered_router(),
            workspace,
            tools,
        );
        let id = service
            .create_session("add a greeting", &serde_json::json!({}))
            .await
            .unwrap();
        let mut sink = CollectingSink::default();
        service.run_prompt(id, "add a greeting", &mut sink).await.unwrap();

        let meters: Vec<_> = sink
            .events
            .iter()
            .filter_map(|e| match e.kind {
                EventKind::TokenCostMeter { input_tokens, output_tokens } => {
                    Some((input_tokens, output_tokens))
                }
                _ => None,
            })
            .collect();
        assert!(!meters.is_empty(), "expected at least one TokenCostMeter event");
        // Cumulative + monotonic non-decreasing.
        for w in meters.windows(2) {
            assert!(w[1].0 >= w[0].0 && w[1].1 >= w[0].1);
        }
    }
```

- [ ] **Step 6: Run engine tests**

Run: `cargo test -p otto-engine && cargo build --workspace`
Expected: PASS. (Existing service tests use the offline `scripted_router` with no usage → no meter events → unchanged; serve still compiles via the `run_prompt_with_approver` wrapper.)

- [ ] **Step 7: Commit**

```bash
cargo fmt --all
git add crates/engine/src/service.rs crates/engine/src/lib.rs
git commit -m "feat(engine): TurnControls + meter wiring via MeteringRouter"
```

---

## Task 11: engine/serve — Pause/Resume routing + integration tests

**Files:**
- Modify: `crates/engine/src/serve.rs` (imports ~6-30, handle_socket ~240-396)
- Modify: `crates/engine/tests/serve.rs` (add a metering server + meter and pause integration tests)

- [ ] **Step 1: Add imports + the pause primitive.** In `crates/engine/src/serve.rs`, add to the imports:

```rust
use std::sync::atomic::{AtomicBool, Ordering};
use otto_engine_core::tool::PauseController;
use tokio::sync::Notify;
use crate::service::TurnControls;
```

After the `InteractiveApprover` impl (~line 213), add:

```rust
/// Connection-scoped pause state: a flag plus a notify to wake parked turns. Shared between the
/// running turn's `InteractivePauser` and the socket reader that routes `Pause`/`Resume`.
#[derive(Default)]
struct PauseState {
    paused: AtomicBool,
    resume: Notify,
}

impl PauseState {
    fn pause(&self) {
        self.paused.store(true, Ordering::SeqCst);
    }
    /// Clear the flag and wake any parked turn (also the disconnect/abort release path).
    fn resume_all(&self) {
        self.paused.store(false, Ordering::SeqCst);
        self.resume.notify_waiters();
    }
}

/// Pause controller backed by a connection's `PauseState`.
struct InteractivePauser(Arc<PauseState>);

#[async_trait::async_trait]
impl PauseController for InteractivePauser {
    fn should_pause(&self) -> bool {
        self.0.paused.load(Ordering::SeqCst)
    }
    async fn wait_for_resume(&self) {
        loop {
            // Arm the notified future BEFORE re-checking the flag, so a Resume that fires between
            // the check and the await is not lost.
            let notified = self.0.resume.notified();
            if !self.0.paused.load(Ordering::SeqCst) {
                return;
            }
            notified.await;
        }
    }
}
```

- [ ] **Step 2: Create the per-connection pause state.** In `handle_socket`, alongside `let approvals = ApprovalRegistry::new();` (~line 302), add:

```rust
    let pause_state = Arc::new(PauseState::default());
```

- [ ] **Step 3: Build the controls and use `run_prompt_with_controls`.** In the `Command::SendPrompt { text, .. } =>` arm, replace the approver construction + the `run_prompt_with_approver` call:

```rust
                let approver = Arc::new(InteractiveApprover::new(approvals.clone()));
                let pauser = Arc::new(InteractivePauser(Arc::clone(&pause_state)));
                let controls = TurnControls { approver, pauser };
                let turn_err = {
                    let mut sink = WsSink {
                        writer: &mut writer,
                    };
                    let turn = state
                        .service
                        .run_prompt_with_controls(session, &text, &mut sink, controls);
                    tokio::pin!(turn);
                    // … (the existing `let mut err …; loop { select! { … } }` block, with the
                    //     inner-match additions in Step 4) …
```

- [ ] **Step 4: Route Pause/Resume in the inner `select!` and release on disconnect/abort.** Inside the inner `match serde_json::from_str::<Command>(t.as_str())` block, add arms (beside `ApproveDiff`/`Abort`):

```rust
                                        Ok(Command::Pause { .. }) => {
                                            pause_state.pause();
                                        }
                                        Ok(Command::Resume { .. }) => {
                                            pause_state.resume_all();
                                        }
```

In the inner `Ok(Command::Abort { .. })` arm, add `pause_state.resume_all();` before `break 'outer;` (so a parked turn unwinds). In the inner `Some(Ok(Message::Close(_))) | None =>` arm, add `pause_state.resume_all();` before `break 'outer;`.

- [ ] **Step 5: Handle Pause/Resume in the OUTER match (no turn in flight).** Replace the temporary no-op arm added in Task 1 with real handling, so a `Pause` sent before `SendPrompt` is honored at the turn's first checkpoint:

```rust
            Command::Pause { .. } => {
                pause_state.pause();
            }
            Command::Resume { .. } => {
                pause_state.resume_all();
            }
```

- [ ] **Step 6: Add integration tests.** In `crates/engine/tests/serve.rs`, add a metering server helper and two tests:

```rust
/// Start a serve app whose router reports token usage (so meter events fire) with ordinary
/// (auto-allowed) writes. Returns the bound port and the tempdir.
async fn start_metering_server() -> (u16, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let provider = ScriptedProvider::new("{}")
        .on(
            "edits",
            r#"{"edits": [{"path": "out.txt", "contents": "hi add a greeting"}]}"#,
        )
        .on("milestones", r#"{"milestones": [{"description": "x"}]}"#)
        .with_usage(10, 20);
    let router: Arc<dyn otto_engine_core::Router> =
        Arc::new(SingleProviderRouter::new(Arc::new(provider)));
    let workspace: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(dir.path()));
    let tools_ws: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(dir.path()));
    let tools = Arc::new(build_tool_registry(tools_ws, dir.path().to_path_buf()));
    let store: Arc<dyn otto_persistence::SessionStore> = Arc::new(
        otto_persistence::SqliteStore::open(dir.path().join("s.db"))
            .await
            .unwrap(),
    );
    let service = EngineService::new(
        store,
        Arc::new(build_default_registry()),
        router,
        workspace,
        tools,
    );
    let app = serve_app(service, TOKEN.to_string(), test_capabilities());
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        serve_run(listener, app, None).await.unwrap();
    });
    (port, dir)
}

#[tokio::test]
async fn streams_token_cost_meter_events() {
    let (port, _dir) = start_metering_server().await;
    let (mut ws, _) = tokio_tungstenite::connect_async(authed_request(port, ""))
        .await
        .expect("connect");
    let ready: Value = next_json(&mut ws).await;
    let session = ready["session"].as_str().unwrap().to_string();

    let cmd = serde_json::json!({ "SendPrompt": { "session": session, "text": "add a greeting" } });
    ws.send(Message::Text(serde_json::to_string(&cmd).unwrap()))
        .await
        .unwrap();

    let mut saw_meter = false;
    while let Some(frame) = next_json_opt(&mut ws).await {
        if frame["type"] == "event" {
            let kind = &frame["event"]["kind"];
            if let Some(m) = kind.get("TokenCostMeter") {
                assert!(m["input_tokens"].as_u64().unwrap() > 0);
                saw_meter = true;
            }
            if kind.get("TurnComplete").is_some() {
                break;
            }
        }
    }
    assert!(saw_meter, "expected at least one TokenCostMeter event");
}

#[tokio::test]
async fn pause_before_prompt_parks_turn_until_resume() {
    let (port, _dir) = start_metering_server().await;
    let (mut ws, _) = tokio_tungstenite::connect_async(authed_request(port, ""))
        .await
        .expect("connect");
    let ready: Value = next_json(&mut ws).await;
    let session = ready["session"].as_str().unwrap().to_string();

    // Pause BEFORE prompting → the turn's first checkpoint parks deterministically.
    let pause = serde_json::json!({ "Pause": { "session": session } });
    ws.send(Message::Text(serde_json::to_string(&pause).unwrap()))
        .await
        .unwrap();
    let cmd = serde_json::json!({ "SendPrompt": { "session": session, "text": "add a greeting" } });
    ws.send(Message::Text(serde_json::to_string(&cmd).unwrap()))
        .await
        .unwrap();

    // Read until "turn paused"; assert TurnComplete did NOT arrive first.
    let mut paused = false;
    while let Some(frame) = next_json_opt(&mut ws).await {
        if frame["type"] == "event" {
            let kind = &frame["event"]["kind"];
            if let Some(msg) = kind.get("Log").and_then(|l| l["message"].as_str()) {
                if msg == "turn paused" {
                    paused = true;
                    break;
                }
            }
            assert!(
                kind.get("TurnComplete").is_none(),
                "turn completed before pausing"
            );
        }
    }
    assert!(paused, "expected a 'turn paused' log");

    // Resume → expect "turn resumed" then TurnComplete.
    let resume = serde_json::json!({ "Resume": { "session": session } });
    ws.send(Message::Text(serde_json::to_string(&resume).unwrap()))
        .await
        .unwrap();
    let mut resumed = false;
    let mut completed = false;
    while let Some(frame) = next_json_opt(&mut ws).await {
        if frame["type"] == "event" {
            let kind = &frame["event"]["kind"];
            if let Some(msg) = kind.get("Log").and_then(|l| l["message"].as_str()) {
                if msg == "turn resumed" {
                    resumed = true;
                }
            }
            if kind.get("TurnComplete").is_some() {
                completed = true;
                break;
            }
        }
    }
    assert!(resumed, "expected a 'turn resumed' log");
    assert!(completed, "expected the turn to complete after resume");
}
```

- [ ] **Step 7: Run the build + tests**

Run: `cargo build --workspace && cargo test -p otto-engine --test serve`
Expected: PASS (new meter + pause tests, and all existing serve tests including the approval ones).

- [ ] **Step 8: Commit**

```bash
cargo fmt --all
git add crates/engine/src/serve.rs crates/engine/tests/serve.rs
git commit -m "feat(engine): serve routes Pause/Resume; meter + pause integration tests"
```

---

## Task 12: UI — decode `TokenCostMeter`, meter readout + cost estimate

The `ui/` crate is standalone (run from inside `ui/`). It depends only on `protocol`.

**Files:**
- Modify: `ui/src/view_model.rs` (add `format_meter`/`cost_estimate` + `describe_event` arm + tests)
- Modify: `ui/src/components/status_line.rs` (add `meter` param + readout)
- Modify: `ui/src/app.rs` (meter signal; update on event; pass to StatusLine)

- [ ] **Step 1: Add pure helpers + the `describe_event` arm with failing tests.** In `ui/src/view_model.rs`, add near the other pure fns:

```rust
/// Approximate per-million-token display rates for the default remote model (claude-haiku-4-5).
/// These drive only the UI cost estimate; update freely — they are not load-bearing.
const COST_PER_MTOK_IN: f64 = 0.80;
const COST_PER_MTOK_OUT: f64 = 4.00;

/// Running token counts for the status strip.
pub fn format_meter(input_tokens: u64, output_tokens: u64) -> String {
    format!("↑{input_tokens} ↓{output_tokens} tok")
}

/// Approximate dollar cost, or `None` when no remote (billable) model is configured — in that
/// case the meter shows tokens only. A clearly-labeled estimate: it applies the remote rate to
/// all counted tokens.
pub fn cost_estimate(input_tokens: u64, output_tokens: u64, remote_llm: bool) -> Option<f64> {
    if !remote_llm {
        return None;
    }
    Some(input_tokens as f64 / 1_000_000.0 * COST_PER_MTOK_IN
        + output_tokens as f64 / 1_000_000.0 * COST_PER_MTOK_OUT)
}
```

In `describe_event`, add an arm (before the closing `}` of the match):

```rust
        EventKind::TokenCostMeter {
            input_tokens,
            output_tokens,
        } => row(
            "row-meter",
            format!("◷ tokens ↑{input_tokens} ↓{output_tokens}"),
        ),
```

Add tests in `mod tests`:

```rust
    #[test]
    fn format_meter_shows_both_counts() {
        assert_eq!(format_meter(12, 34), "↑12 ↓34 tok");
    }

    #[test]
    fn cost_is_none_without_remote_model() {
        assert_eq!(cost_estimate(1_000, 1_000, false), None);
    }

    #[test]
    fn cost_uses_remote_rates() {
        let c = cost_estimate(1_000_000, 1_000_000, true).unwrap();
        assert!((c - (0.80 + 4.00)).abs() < 1e-9);
    }

    #[test]
    fn describe_token_cost_meter_row() {
        let r = describe_event(&EventKind::TokenCostMeter {
            input_tokens: 7,
            output_tokens: 9,
        });
        assert_eq!(r.class, "row-meter");
        assert!(r.text.contains("↑7"));
    }
```

- [ ] **Step 2: Run the host tests to verify they pass after the impl above**

Run: `cd ui && cargo test view_model::`
Expected: PASS (the impl in Step 1 satisfies the tests; `describe_event` is now exhaustive again).

- [ ] **Step 3: Add the meter readout to `StatusLine`.** In `ui/src/components/status_line.rs`, update the import and signature, and add the readout. Change the `use` to include the helpers:

```rust
use crate::view_model::{capability_segments, cost_estimate, format_meter, short_session, status_label, ConnState};
```

Add a `meter` param:

```rust
#[component]
pub fn StatusLine(
    conn: RwSignal<ConnState>,
    last_seq: RwSignal<Option<u64>>,
    capabilities: RwSignal<Option<CapabilitiesManifest>>,
    meter: RwSignal<Option<(u64, u64)>>,
) -> impl IntoView {
```

Inside the `<div class="status">`, after the capability-group block, add:

```rust
            {move || {
                let connected = matches!(conn.get(), ConnState::Connected { .. });
                meter.get().filter(|_| connected).map(|(i, o)| {
                    let remote = capabilities.get().map(|m| m.remote_llm).unwrap_or(false);
                    let text = match cost_estimate(i, o, remote) {
                        Some(c) => format!("{} · ~${:.4}", format_meter(i, o), c),
                        None => format_meter(i, o),
                    };
                    view! { <span class="cap-sep">" | "</span><span class="meter">{text}</span> }
                })
            }}
```

- [ ] **Step 4: Wire the meter signal in `app.rs`.** In `ui/src/app.rs`:

Add the signal (near the other stream-state signals, ~line 32):

```rust
    let meter = RwSignal::new(None::<(u64, u64)>); // (input, output) tokens for the current turn
```

In the `Ok(ServerMessage::Event { event })` handler, alongside the `ApprovalRequest` branch, update the meter:

```rust
                    if let EventKind::TokenCostMeter { input_tokens, output_tokens } = &event.kind {
                        meter.set(Some((*input_tokens, *output_tokens)));
                    }
```

Reset the meter when a new turn starts and on disconnect: in `send_prompt` (before sending) add `meter.set(None);`; in `connect` (near `pending_approval.set(None);`) add `meter.set(None);`; in `disconnect` and `on_close`/`on_error` add `meter.set(None);`.

Pass it to `StatusLine`:

```rust
            <StatusLine conn=conn last_seq=last_seq capabilities=capabilities meter=meter />
```

- [ ] **Step 5: Build for wasm + run host tests**

Run: `cd ui && cargo test && cargo build --target wasm32-unknown-unknown`
Expected: PASS; wasm compiles.

- [ ] **Step 6: Commit**

```bash
cargo fmt --all
git add ui/src/view_model.rs ui/src/components/status_line.rs ui/src/app.rs
git commit -m "feat(ui): token/cost meter readout in the status strip"
```

---

## Task 13: UI — Pause/Resume control

**Files:**
- Modify: `ui/src/components/prompt_bar.rs` (add Pause/Resume button)
- Modify: `ui/src/app.rs` (paused signal + pause/resume senders; pass to PromptBar)

- [ ] **Step 1: Extend `PromptBar`.** In `ui/src/components/prompt_bar.rs`, add params and a toggle button:

```rust
#[component]
pub fn PromptBar(
    conn: RwSignal<ConnState>,
    paused: RwSignal<bool>,
    on_send: Callback<String>,
    on_abort: Callback<()>,
    on_pause: Callback<()>,
    on_resume: Callback<()>,
) -> impl IntoView {
    let text = RwSignal::new(String::new());
    let connected = move || matches!(conn.get(), ConnState::Connected { .. });

    let send = move |_| {
        let t = text.get();
        if !t.trim().is_empty() {
            on_send.run(t);
            text.set(String::new());
        }
    };

    view! {
        <div class="prompt">
            <input
                class="prompt-input"
                type="text"
                placeholder="prompt…"
                prop:value=move || text.get()
                on:input=move |e| text.set(event_target_value(&e))
                disabled=move || !connected()
            />
            <button on:click=send disabled=move || !connected()>"Send"</button>
            <button
                on:click=move |_| if paused.get() { on_resume.run(()) } else { on_pause.run(()) }
                disabled=move || !connected()
            >
                {move || if paused.get() { "Resume" } else { "Pause" }}
            </button>
            <button
                on:click=move |_| on_abort.run(())
                disabled=move || !connected()
            >"Abort"</button>
        </div>
    }
}
```

- [ ] **Step 2: Wire pause state + senders in `app.rs`.** In `ui/src/app.rs`:

Add the signal near `meter`:

```rust
    let paused = RwSignal::new(false);
```

Add sender closures (after `abort`):

```rust
    let pause = move || {
        let (Some(ws), Some(sid)) = (socket.get(), session.get()) else { return; };
        if let Ok(uuid) = Uuid::parse_str(&sid) {
            let _ = send_command(&ws, &Command::Pause { session: SessionId(uuid) });
            paused.set(true);
        }
    };
    let resume = move || {
        let (Some(ws), Some(sid)) = (socket.get(), session.get()) else { return; };
        if let Ok(uuid) = Uuid::parse_str(&sid) {
            let _ = send_command(&ws, &Command::Resume { session: SessionId(uuid) });
            paused.set(false);
        }
    };
```

Reset `paused` on new turn/disconnect: add `paused.set(false);` in `send_prompt` (before sending), in `connect`, in `disconnect`, and in `on_close`/`on_error`.

Pass to `PromptBar`:

```rust
            <PromptBar
                conn=conn
                paused=paused
                on_send=Callback::new(send_prompt)
                on_abort=Callback::new(move |_| abort())
                on_pause=Callback::new(move |_| pause())
                on_resume=Callback::new(move |_| resume())
            />
```

- [ ] **Step 3: Build for wasm + run host tests**

Run: `cd ui && cargo test && cargo build --target wasm32-unknown-unknown`
Expected: PASS; wasm compiles.

- [ ] **Step 4: Commit**

```bash
cargo fmt --all
git add ui/src/components/prompt_bar.rs ui/src/app.rs
git commit -m "feat(ui): Pause/Resume control for an in-flight turn"
```

---

## Task 14: Docs — record sub-project E shipped

**Files:**
- Modify: `docs/superpowers/specs/2026-06-17-ui-roadmap.md` (status line ~4; row E ~42)

- [ ] **Step 1: Update the roadmap status.** In `docs/superpowers/specs/2026-06-17-ui-roadmap.md`, extend the `**Status:**` line (line 4) to note E shipped with links to its design + plan, mirroring the D entry, and change `E–F pending` → `F pending`.

- [ ] **Step 2: Mark row E shipped.** In the sub-projects table, change the `| **E** |` cell to `| **E** ✅ |` with `*(shipped — [design](2026-06-19-ui-token-meter-pause-resume-design.md) · [plan](../plans/2026-06-19-ui-token-meter-pause-resume.md))*`, and replace the "Protocol / engine changes" cell with a **Done:** summary: `Pause`/`Resume` commands + `TokenCostMeter` event (additive, semver-minor); `Usage` on `CompleteResponse` reported by Anthropic/Ollama; a `MeteringRouter` decorator tallying into a per-turn `TokenMeter`; orchestrator emits cumulative `TokenCostMeter` at phase boundaries (offline emits none); a `PauseController` seam (`NeverPause` default) checked at phase boundaries; serve routes `Pause`/`Resume` through the existing `select!` over a connection-scoped `PauseState`, releasing on disconnect/abort; the UI shows a token/cost meter and a Pause/Resume button.

- [ ] **Step 3: Verify the whole suite once more**

Run: `cargo test --workspace && (cd ui && cargo test && cargo build --target wasm32-unknown-unknown)`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add docs/superpowers/specs/2026-06-17-ui-roadmap.md
git commit -m "docs: record sub-project E (token meter + pause/resume) shipped"
```

---

## Done-when

- `Pause`/`Resume`/`TokenCostMeter` round-trip in `protocol`.
- Anthropic + Ollama report `Usage`; `LocalProvider`/`ScriptedProvider` report `None`.
- `MeteringRouter` tallies usage; the orchestrator emits cumulative `TokenCostMeter` at phase boundaries and **emits none when usage is absent** (offline event stream byte-for-byte unchanged; the existing exact-sequence orchestrator test passes untouched aside from the new fields).
- A turn pauses at the next phase boundary on `Pause` and resumes on `Resume`; disconnect/abort release a parked turn (no hang).
- Serve integration tests prove meter events stream and pause→resume brackets the turn.
- The UI shows running tokens + an approximate `~$` cost and a working Pause/Resume button; `cd ui && cargo test` and the wasm build pass.
- `cargo test --workspace` is green; CLI/offline behavior unchanged.
