# otto Plan 4c — Orchestrator Repair Loop Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Complete the deterministic spine's `Repair` state — when the Verifier reports failure, the orchestrator re-runs the Coder with the failure as feedback and re-verifies, up to a bounded number of attempts, escalating the routing each time.

**Architecture:** The orchestrator's `Code → apply (gated) → Verify` sequence becomes a bounded loop. `AgentRequest::Code` gains `feedback: Option<String>` (the previous verify failure) and `prior_failures: u32`; the Coder threads `feedback` into its prompt and `prior_failures` into its `RouteHints` (so Brain-Blend escalates local→remote on repeated failure). On a verify failure the orchestrator increments `prior_failures`, sets `feedback` to the failure detail, emits a "repairing" log, and loops; it stops on success or after `MAX_REPAIRS`, reporting the final outcome. Fully deterministic and testable with mock agents; the gate (Plan 4a) still guards every edit on every attempt.

**Tech Stack:** Rust (edition 2024), serde_json, async-trait, anyhow.

---

## Context for the implementer (read once)

Current state (`main`):
- `crates/engine-core/src/types.rs`: `pub enum AgentRequest { Plan { goal }, FindContext { goal }, Code { goal: String, context: Vec<PathBuf> }, Verify }`. `AgentOutput::Verify { ok: bool, detail: String }`.
- `crates/engine-core/src/router.rs`: `RouteHints { task_kind, token_estimate, privacy_sensitive, prior_failures }` (all `Default`). The `otto-router::decide_route` policy routes to Remote when `prior_failures >= 2`.
- `crates/engine-core/src/orchestrator.rs` `run_turn`: after ContextFinder produces `files`, it runs the Coder once (`AgentRequest::Code { goal, context: files }`), applies each edit **through the fail-closed gate** (`tools.check("fs.write", {path}) != Decision::Allow` → log + skip; else `workspace.apply_edit`), runs the Verifier once, emits `VerifyResult { ok, detail }`, then `TurnComplete { ok }`. Imports include `use crate::tool::{Decision, ToolRegistry};` and use `serde_json::json!` fully-qualified.
- `crates/agents/src/coder.rs`: the real `Coder` destructures `AgentRequest::Code { goal, context }`, builds `code_prompt(&goal, &context)`, calls `ctx.router().complete(prompt, RouteHints { task_kind: TaskKind::Edit, ..default })`, parses edits via `extract_json`, falls back to no edits. The prompt is PROSE (no literal JSON example) and contains "edits", not "milestones".
- `StubVerifier` always returns `{ ok: true }`, so the repair loop is dormant in production until the real Verifier lands (Plan 4c-2) — this plan delivers the loop architecture + tests, consistent with otto's seam-before-consumer pattern.

**Conventions:** stay on branch `feat/plan-4c-repair-loop`; never detach HEAD; `git add`+`commit` only (no `--amend`); no AI/Claude self-attribution; per-package then workspace gates; `clippy -D warnings` clean; TDD. The `impl Agent` lifetime rule: `ctx: &AgentCtx` (never `<'_>`). The happy-path full-ordered-event test in `orchestrator.rs` must stay PASSING and unchanged (a turn whose Verifier passes first try produces exactly the same event sequence as today).

---

## File Structure

```
crates/engine-core/src/
├── types.rs         # MODIFY: AgentRequest::Code gains feedback + prior_failures
└── orchestrator.rs  # MODIFY: Code construction (Task 1) → Repair loop (Task 3) + tests
crates/agents/src/
└── coder.rs         # MODIFY: consume feedback (prompt) + prior_failures (RouteHints) + tests
docs/ARCHITECTURE.md # MODIFY: document the Repair loop
```

---

## Task 1: Extend `AgentRequest::Code`; plumb through the orchestrator

**Files:**
- Modify: `crates/engine-core/src/types.rs`
- Modify: `crates/engine-core/src/orchestrator.rs`

EXPECTED partial breakage: adding fields to `Code` breaks the `agents` Coder (it destructures the old shape) and therefore `otto-engine`. Fixed in Task 2. In THIS task only build/test `-p otto-engine-core`.

- [ ] **Step 1: Add the two fields to `AgentRequest::Code`**

In `crates/engine-core/src/types.rs`, change the `Code` variant:

```rust
    Code {
        goal: String,
        context: Vec<PathBuf>,
        /// The previous verify failure detail, if this is a repair attempt.
        feedback: Option<String>,
        /// How many times this turn has already failed verification (drives routing escalation).
        prior_failures: u32,
    },
```

(Leave `Plan`, `FindContext`, `Verify` unchanged.)

- [ ] **Step 2: Update the orchestrator's single Code construction**

In `crates/engine-core/src/orchestrator.rs`, the Execute phase constructs `AgentRequest::Code { goal: goal.to_string(), context: files }`. Change it to include the new fields with first-attempt values:

```rust
                AgentRequest::Code {
                    goal: goal.to_string(),
                    context: files,
                    feedback: None,
                    prior_failures: 0,
                },
```

(No behavior change yet — the repair loop is added in Task 3. The inline test mock agents destructure `AgentRequest` with `_req` / `..`, so they are unaffected.)

- [ ] **Step 3: Build & test engine-core only**

Run: `cargo test -p otto-engine-core` (all pass — the happy-path ordered-event test still asserts the same 12 events), `cargo clippy -p otto-engine-core --all-targets -- -D warnings` (clean), `cargo fmt -p otto-engine-core` (clean). Do NOT run `--workspace` (agents/engine broken until Task 2).

- [ ] **Step 4: Commit**

```bash
git add crates/engine-core/src/types.rs crates/engine-core/src/orchestrator.rs
git commit -m "feat(engine-core): AgentRequest::Code carries feedback + prior_failures"
```

---

## Task 2: Coder consumes `feedback` + `prior_failures`

**Files:**
- Modify: `crates/agents/src/coder.rs`

- [ ] **Step 1: Thread feedback into the prompt and prior_failures into routing**

In `crates/agents/src/coder.rs`, change `code_prompt` to accept optional feedback and append a repair instruction when present:

```rust
fn code_prompt(goal: &str, context: &[PathBuf], feedback: Option<&str>) -> String {
    let files = if context.is_empty() {
        "(none)".to_string()
    } else {
        context
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut prompt = format!(
        "You are otto's coder. Produce the complete file edits that accomplish the goal.\n\
         Goal: {goal}\n\
         Existing files: {files}\n\
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

Update the `Coder::run` body to destructure the new fields and use them:

```rust
    async fn run(&self, req: AgentRequest, ctx: &AgentCtx) -> anyhow::Result<AgentOutput> {
        let AgentRequest::Code { goal, context, feedback, prior_failures } = req else {
            anyhow::bail!("Coder received a non-Code request");
        };
        let completion = ctx
            .router()
            .complete(
                CompleteRequest { prompt: code_prompt(&goal, &context, feedback.as_deref()) },
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
                .map(|e| Edit { path: PathBuf::from(e.path), new_contents: e.contents })
                .collect(),
            Err(_) => Vec::new(),
        };
        Ok(AgentOutput::Code { edits })
    }
```

- [ ] **Step 2: Update the existing coder tests for the new Code shape + add a feedback test**

In `crates/agents/src/coder.rs` test module, the `run_coder` helper builds `AgentRequest::Code { goal, context: Vec::new() }`. Update it to include the new fields, and parameterize feedback so a new test can exercise it:

```rust
    async fn run_coder_with(router: &SingleProviderRouter, feedback: Option<String>) -> Vec<Edit> {
        let dir = tempfile::tempdir().unwrap();
        let ws = LocalWorkspace::new(dir.path());
        let tools = ToolRegistry::new(Arc::new(DefaultPermissionGate::new()), Arc::new(DenyAsk));
        let ctx = AgentCtx::new(router, &ws, &tools);
        let out = Coder
            .run(
                AgentRequest::Code {
                    goal: "add a greeting".to_string(),
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
```

Update the two existing tests to call `run_coder_with(&router, None)` instead of `run_coder(&router)`:

```rust
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
```

Add a new test proving `feedback` reaches the prompt — a `ScriptedProvider` keyed on a unique feedback marker only returns the edit JSON if the marker is in the prompt:

```rust
    #[tokio::test]
    async fn feedback_is_included_in_the_prompt() {
        // The scripted rule fires only if the prompt contains the feedback marker; otherwise
        // the default ("{}") yields no edits. So a non-empty result proves the feedback was
        // threaded into the prompt.
        let provider = ScriptedProvider::new("{}").on(
            "MARKER-9F3",
            r#"{"edits": [{"path": "fixed.txt", "contents": "ok"}]}"#,
        );
        let router = SingleProviderRouter::new(Arc::new(provider));
        let edits = run_coder_with(&router, Some("error: MARKER-9F3 something broke".into())).await;
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].path, PathBuf::from("fixed.txt"));
    }
```

- [ ] **Step 3: Test**

Run: `cargo test -p otto-agents` (coder tests `parses_edits_from_json`, `falls_back_to_no_edits_when_unparseable`, `feedback_is_included_in_the_prompt` pass; planner/parse/context_finder still pass), `cargo clippy -p otto-agents --all-targets -- -D warnings` (clean), `cargo fmt -p otto-agents` (clean). (Do NOT run `--workspace` — the orchestrator construction was updated in Task 1, so the workspace should actually compile now; you MAY run `cargo build --workspace` to confirm — it should succeed since Task 1 added the fields and this task consumes them. If it compiles, run `cargo test --workspace` and confirm green.)

- [ ] **Step 4: Commit**

```bash
git add crates/agents/src/coder.rs
git commit -m "feat(agents): Coder threads feedback into prompt and prior_failures into routing"
```

---

## Task 3: The orchestrator Repair loop

**Files:**
- Modify: `crates/engine-core/src/orchestrator.rs`

This is the spine change. Wrap the Code→apply→Verify sequence in a bounded loop. The happy path (Verifier passes first try) must emit the SAME events as today.

- [ ] **Step 1: Replace the Execute(Coder)+Verify section with the repair loop**

In `crates/engine-core/src/orchestrator.rs` `run_turn`, the ContextFinder section is unchanged. Replace everything from the `// --- Execute ---` Coder block through the `// --- Verify ---` block and the final `TurnComplete`/`Ok` with the loop below. (Keep the Planner and ContextFinder phases exactly as they are; this replaces the Coder + Verify + Done tail.)

The new tail of `run_turn` (after `files` is bound from the ContextFinder output):

```rust
        // --- Code -> apply (gated) -> Verify -> (Repair) ---
        // On verify failure, re-run the Coder with the failure as feedback and re-verify,
        // up to MAX_REPAIRS attempts, escalating routing via prior_failures each time.
        const MAX_REPAIRS: u32 = 2;
        let mut prior_failures: u32 = 0;
        let mut feedback: Option<String> = None;

        let ok = loop {
            // Coder
            emit.emit(EventKind::AgentStarted { role: Role::Coder });
            let coded = self
                .registry
                .get(&Role::Coder)?
                .run(
                    AgentRequest::Code {
                        goal: goal.to_string(),
                        context: files.clone(),
                        feedback: feedback.clone(),
                        prior_failures,
                    },
                    &ctx,
                )
                .await?;
            let AgentOutput::Code { edits } = coded else {
                anyhow::bail!("coder returned unexpected output");
            };
            for edit in &edits {
                let check_args = serde_json::json!({ "path": edit.path.to_string_lossy() });
                if self.tools.check("fs.write", &check_args) != Decision::Allow {
                    emit.emit(EventKind::Log {
                        message: format!(
                            "edit to {} denied by permission gate; skipped",
                            edit.path.display()
                        ),
                    });
                    continue;
                }
                let bytes_written = self.workspace.apply_edit(edit).await?;
                emit.emit(EventKind::FileEdit {
                    path: edit.path.clone(),
                    bytes_written,
                });
            }
            emit.emit(EventKind::AgentFinished { role: Role::Coder });

            // Verify
            emit.emit(EventKind::AgentStarted { role: Role::Verifier });
            let verified = self
                .registry
                .get(&Role::Verifier)?
                .run(AgentRequest::Verify, &ctx)
                .await?;
            let AgentOutput::Verify { ok, detail } = verified else {
                anyhow::bail!("verifier returned unexpected output");
            };
            emit.emit(EventKind::VerifyResult { ok, detail: detail.clone() });
            emit.emit(EventKind::AgentFinished { role: Role::Verifier });

            if ok {
                break true;
            }
            if prior_failures >= MAX_REPAIRS {
                break false;
            }
            prior_failures += 1;
            feedback = Some(detail);
            emit.emit(EventKind::Log {
                message: format!("verify failed; repairing (attempt {prior_failures})"),
            });
        };

        // --- Done ---
        emit.emit(EventKind::TurnComplete { ok });
        Ok(TurnOutcome { ok })
```

(Note `context: files.clone()` and `feedback: feedback.clone()` — the loop may run multiple times, so these are cloned per iteration. `files` is `Vec<PathBuf>` and `feedback` is `Option<String>` — cheap.)

- [ ] **Step 2: Confirm the happy-path test is unchanged & passing**

Run: `cargo test -p otto-engine-core orchestrator::run_turn_drives_full_spine_and_emits_ordered_events`
Expected: PASS. The `OkVerifier` returns `ok: true`, so the loop runs exactly once and breaks `true` — the emitted events are identical to before (Coder started, FileEdit, Coder finished, Verifier started, VerifyResult{ok:true}, Verifier finished, TurnComplete{ok:true}). The 12-element ordered-event assertion still holds.

- [ ] **Step 3: Add repair-success and repair-exhaustion tests**

In the `#[cfg(test)] mod tests` of `orchestrator.rs`, add a stateful flaky verifier and an always-fail verifier (the test module already has `async_trait`, `Arc`, `AgentOutput`, `AgentRequest`, the `registry()` helper with `OneEditCoder`, `FakeRouter`, `RecordingWorkspace`, `empty_tools()`). Add the imports `use std::sync::atomic::{AtomicU32, Ordering};` to the test module. Add:

```rust
    /// Fails verification `fails` times, then succeeds.
    struct FlakyVerifier {
        fails_remaining: AtomicU32,
    }
    #[async_trait]
    impl Agent for FlakyVerifier {
        async fn run(&self, _req: AgentRequest, _ctx: &AgentCtx) -> anyhow::Result<AgentOutput> {
            let remaining = self.fails_remaining.load(Ordering::SeqCst);
            if remaining > 0 {
                self.fails_remaining.store(remaining - 1, Ordering::SeqCst);
                Ok(AgentOutput::Verify { ok: false, detail: "boom".into() })
            } else {
                Ok(AgentOutput::Verify { ok: true, detail: "ok".into() })
            }
        }
    }

    struct AlwaysFailVerifier;
    #[async_trait]
    impl Agent for AlwaysFailVerifier {
        async fn run(&self, _req: AgentRequest, _ctx: &AgentCtx) -> anyhow::Result<AgentOutput> {
            Ok(AgentOutput::Verify { ok: false, detail: "still broken".into() })
        }
    }

    fn registry_with_verifier(verifier: Arc<dyn Agent>) -> AgentRegistry {
        let mut r = AgentRegistry::new();
        r.register(Role::Planner, Arc::new(FixedPlanner));
        r.register(Role::ContextFinder, Arc::new(EmptyContextFinder));
        r.register(Role::Coder, Arc::new(OneEditCoder));
        r.register(Role::Verifier, verifier);
        r
    }

    fn collect_events() -> (Arc<Mutex<Vec<EventKind>>>, impl Fn(EventKind)) {
        let events: Arc<Mutex<Vec<EventKind>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = {
            let events = Arc::clone(&events);
            move |kind: EventKind| events.lock().unwrap().push(kind)
        };
        (events, sink)
    }

    #[tokio::test]
    async fn flaky_verifier_triggers_repair_then_succeeds() {
        let reg = registry_with_verifier(Arc::new(FlakyVerifier {
            fails_remaining: AtomicU32::new(1),
        }));
        let router = FakeRouter;
        let workspace = RecordingWorkspace::default();
        let tools = empty_tools();
        let orch = Orchestrator {
            registry: &reg,
            router: &router,
            workspace: &workspace,
            tools: &tools,
        };
        let (events, sink) = collect_events();

        let outcome = orch.run_turn(SessionId::new(), "x", &sink).await.unwrap();
        assert_eq!(outcome, TurnOutcome { ok: true });

        let recorded = events.lock().unwrap().clone();
        // Two Coder cycles (initial + one repair).
        let coder_starts = recorded
            .iter()
            .filter(|e| matches!(e, EventKind::AgentStarted { role: Role::Coder }))
            .count();
        assert_eq!(coder_starts, 2);
        // A "repairing" log was emitted.
        assert!(recorded.iter().any(|e| matches!(
            e,
            EventKind::Log { message } if message.contains("repairing")
        )));
        // First verify failed, then succeeded.
        assert!(recorded.contains(&EventKind::VerifyResult { ok: false, detail: "boom".into() }));
        assert_eq!(recorded.last(), Some(&EventKind::TurnComplete { ok: true }));
    }

    #[tokio::test]
    async fn repair_exhaustion_fails_the_turn() {
        let reg = registry_with_verifier(Arc::new(AlwaysFailVerifier));
        let router = FakeRouter;
        let workspace = RecordingWorkspace::default();
        let tools = empty_tools();
        let orch = Orchestrator {
            registry: &reg,
            router: &router,
            workspace: &workspace,
            tools: &tools,
        };
        let (events, sink) = collect_events();

        let outcome = orch.run_turn(SessionId::new(), "x", &sink).await.unwrap();
        assert_eq!(outcome, TurnOutcome { ok: false });

        let recorded = events.lock().unwrap().clone();
        // MAX_REPAIRS = 2 → 3 total attempts (initial + 2 repairs).
        let coder_starts = recorded
            .iter()
            .filter(|e| matches!(e, EventKind::AgentStarted { role: Role::Coder }))
            .count();
        assert_eq!(coder_starts, 3);
        let repairs = recorded
            .iter()
            .filter(|e| matches!(e, EventKind::Log { message } if message.contains("repairing")))
            .count();
        assert_eq!(repairs, 2);
        assert_eq!(recorded.last(), Some(&EventKind::TurnComplete { ok: false }));
    }
```

(If `FixedPlanner`/`EmptyContextFinder`/`OneEditCoder`/`Mutex` are not already imported in the test module, they are defined in the same module already — reuse them. `registry()` already exists; `registry_with_verifier` is the new variant. `collect_events` factors out the sink boilerplate the existing tests inline — if you prefer, inline it instead, but keep the existing happy-path test working.)

- [ ] **Step 4: Test**

Run: `cargo test -p otto-engine-core` (all pass: the happy-path ordered-event test + `flaky_verifier_triggers_repair_then_succeeds` + `repair_exhaustion_fails_the_turn` + the others), `cargo clippy -p otto-engine-core --all-targets -- -D warnings` (clean), `cargo fmt -p otto-engine-core` (clean), then `cargo test --workspace` (all pass — the engine integration test still passes: `StubVerifier` returns ok first try, so the loop runs once and the parsed edit is written exactly as before).

- [ ] **Step 5: Commit**

```bash
git add crates/engine-core/src/orchestrator.rs
git commit -m "feat(engine-core): bounded Repair loop — re-Code on verify failure with escalation"
```

---

## Task 4: Docs + quality gate

**Files:**
- Modify: `docs/ARCHITECTURE.md`

- [ ] **Step 1: Document the Repair loop**

In `docs/ARCHITECTURE.md`, find the "### The orchestrator spine" content (or the orchestrator description; if the heading differs, place this near the Plan→Execute→Verify→Done description). Append:

```markdown
**Repair.** The `Code → apply (gated) → Verify` step is a bounded loop. On a verify failure the
orchestrator increments `prior_failures`, sets the Coder's `feedback` to the failure detail,
emits a "repairing" log, and re-runs the Coder + Verifier — up to `MAX_REPAIRS` (2) attempts
(3 total). `prior_failures` flows into the Coder's `RouteHints`, so Brain-Blend escalates
local→remote on repeated failure. The turn's outcome is the last Verify result. The happy path
(Verifier passes first try) runs the loop exactly once, so its event sequence is unchanged.
(`StubVerifier` always passes, so the loop is dormant until the real bash-backed Verifier lands.)
```

Also update the `CLAUDE.md` "orchestrator spine" sentence if it lists the phases, to mention the Repair loop (optional — only if the wording is now inaccurate; keep it short).

- [ ] **Step 2: Final gate**

Run: `cargo fmt --all -- --check` (clean), `cargo clippy --workspace --all-targets -- -D warnings` (clean), `cargo test --workspace` — capture the per-crate breakdown and summed total.

- [ ] **Step 3: Commit**

```bash
git add docs/ARCHITECTURE.md CLAUDE.md
git commit -m "docs: document the orchestrator Repair loop"
```

(If you didn't change `CLAUDE.md`, omit it from the `git add`.)

---

## Done — what Plan 4c delivers

The orchestrator's `Repair` state is real: a verify failure triggers a re-Code-and-re-Verify cycle with the failure fed back to the Coder, bounded at 3 attempts, escalating routing via `prior_failures`. The Coder now accepts and uses repair feedback. The gate guards every edit on every attempt. The happy path is unchanged; the loop is exercised by mock-verifier tests. In production it stays dormant until a Verifier that can fail exists.

**Next — Plan 4c-2 (the real Verifier):** run the project's build/test (e.g. `cargo build`/`cargo check`) via the sandboxed `bash` tool and report pass/fail — which requires resolving the toolchain-in-sandbox problem (the cleared-env hardening vs. cargo needing `CARGO_HOME`/`RUSTUP_HOME`/`~/.cargo/bin`); pass through a curated, non-secret toolchain env allowlist, with a security review. Once it lands, this Repair loop activates automatically. Then Plan 4d: real ContextFinder + retrieval.
