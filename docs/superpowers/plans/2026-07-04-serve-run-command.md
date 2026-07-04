# `Command::RunCommand` on `otto serve` Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let a client connected to `otto serve` run a discovered `.claude/commands/*.md` command by name (`Command::RunCommand`), with the exact same template-expansion, tool-narrowing, and model-pinning behavior `otto run --command` already has on the CLI — closing the "command" half of the long-deferred "`--agent`/`--command` subpaths on serve" thread.

**Architecture:** A new `Command::RunCommand { session, name, args }` protocol variant is resolved entirely inside `EngineService` (a new `run_command_with_controls` method: look up the command by name in extensions discovered once at server startup, narrow the tool registry to `allowed-tools`, expand `$ARGUMENTS`/`$1..$9` then resolve `!bash`/`@file` injections, pin the router to `model`), then runs as an ordinary turn through the existing `run_prompt_with_controls` — which gains two new optional per-call overrides (`tools`, `router`) on `TurnControls` so this one call can use a narrowed/pinned pair without disturbing the server's own long-lived defaults. `serve.rs`'s WebSocket handler dispatches to it exactly the way it dispatches `SendPrompt` today (same pause/resume/approval/abort racing, factored into a shared helper so the two can never drift apart).

**Tech Stack:** Rust workspace (`otto-protocol`, `otto-engine`, `otto-extensions` crates), tokio, axum WebSockets, `anyhow`.

**Design spec:** `docs/superpowers/specs/2026-07-04-serve-run-command-design.md` — read it first for the full rationale (in particular, why this is a serve-only, strictly-more-correct improvement over the CLI's `PermissionRules::default()` gap, and why `--agent` is a separate, later plan).

---

### Task 1: Protocol — `Command::RunCommand`

**Files:**
- Modify: `crates/protocol/src/lib.rs:38-71` (the `Command` enum)
- Modify: `crates/protocol/src/lib.rs` (test module, after `command_round_trips_through_json`, currently ending around line 287)

- [ ] **Step 1: Add the new variant**

In `crates/protocol/src/lib.rs`, inside `pub enum Command { ... }`, add a new variant right after `SendPrompt`:

```rust
pub enum Command {
    CreateSession,
    SendPrompt {
        session: SessionId,
        text: String,
    },
    /// Run a discovered `.claude/commands/*.md` command by name: template-expand
    /// `$ARGUMENTS`/`$1..$9` from `args`, resolve `!bash`/`@file` injections through a tool
    /// registry narrowed to the command's `allowed-tools`, then run the result as a normal
    /// turn with the router pinned to the command's `model`. Unknown `name` or an injection
    /// failure surfaces as `ServerMessage::Error` — no turn starts, no `seq` is consumed.
    RunCommand {
        session: SessionId,
        name: String,
        args: Vec<String>,
    },
    Abort {
        session: SessionId,
    },
    // ...rest of the enum is unchanged (ApproveDiff, Pause, Resume, PromoteToRemote, DemoteToLocal)
```

Leave every other variant untouched.

- [ ] **Step 2: Write the round-trip test**

Add this test to the `#[cfg(test)] mod tests` block, right after `command_round_trips_through_json` (around line 287):

```rust
    #[test]
    fn run_command_command_round_trips() {
        let cmd = Command::RunCommand {
            session: SessionId::new(),
            name: "git:commit".to_string(),
            args: vec!["fix bug".to_string()],
        };
        let json = serde_json::to_string(&cmd).unwrap();
        let back: Command = serde_json::from_str(&json).unwrap();
        assert_eq!(cmd, back);
        // External tagging matches the rest of Command (e.g. {"RunCommand":{...}}).
        assert!(json.contains("\"RunCommand\""));
    }
```

- [ ] **Step 3: Run the tests**

Run: `cargo test -p otto-protocol`
Expected: all tests pass, including the new `run_command_command_round_trips`.

- [ ] **Step 4: Commit**

```bash
git add crates/protocol/src/lib.rs
git commit -m "feat(protocol): add Command::RunCommand"
```

---

### Task 2: `TurnControls` gains per-call `tools`/`router` overrides

**Files:**
- Modify: `crates/engine/src/service.rs:44-56` (`TurnControls` struct + `Default` impl)
- Modify: `crates/engine/src/service.rs:146-194` (`run_prompt_with_controls`)
- Modify: `crates/engine/src/serve.rs:500-503` (the one existing `TurnControls { .. }` literal)
- Modify: `crates/engine/src/service.rs` (test module — two new tests)

- [ ] **Step 1: Extend the struct**

In `crates/engine/src/service.rs`, replace:

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

with:

```rust
/// The per-turn control handles the serve layer can inject: how `Ask` edits are approved, how
/// the turn is paused, and (for a `Command::RunCommand` turn) a narrowed tool registry / pinned
/// router that apply to THIS call only — the service's own long-lived `tools`/`router` are never
/// mutated. `Default` is the headless/CLI posture (deny approvals, never pause, no overrides).
pub struct TurnControls {
    pub approver: Arc<dyn Approver>,
    pub pauser: Arc<dyn PauseController>,
    pub tools: Option<Arc<ToolRegistry>>,
    pub router: Option<Arc<dyn Router>>,
}

impl Default for TurnControls {
    fn default() -> Self {
        Self {
            approver: Arc::new(DenyApprover),
            pauser: Arc::new(NeverPause),
            tools: None,
            router: None,
        }
    }
}
```

- [ ] **Step 2: Resolve the overrides in `run_prompt_with_controls`**

In the same file, inside `run_prompt_with_controls`, find this block (it builds the spawned turn task):

```rust
        let handle = {
            let registry = Arc::clone(&self.registry);
            let router = Arc::clone(&self.router);
            let workspace = Arc::clone(&self.workspace);
            let tools = Arc::clone(&self.tools);
            let retriever = self.retriever.clone();
            let goal = goal.to_string();
            let counter = Arc::new(AtomicU64::new(start_seq));
            let approver = Arc::clone(&controls.approver);
            let pauser = Arc::clone(&controls.pauser);
```

Replace the `router`/`tools` lines with the resolved overrides:

```rust
        let handle = {
            let registry = Arc::clone(&self.registry);
            let router = controls.router.clone().unwrap_or_else(|| Arc::clone(&self.router));
            let workspace = Arc::clone(&self.workspace);
            let tools = controls.tools.clone().unwrap_or_else(|| Arc::clone(&self.tools));
            let retriever = self.retriever.clone();
            let goal = goal.to_string();
            let counter = Arc::new(AtomicU64::new(start_seq));
            let approver = Arc::clone(&controls.approver);
            let pauser = Arc::clone(&controls.pauser);
```

Nothing else in the method changes — `router`/`tools` are used exactly as before further down (`MeteringRouter::new(router, ...)`, `Orchestrator { tools: &tools, ... }`).

- [ ] **Step 3: Fix the one call site that constructs `TurnControls` directly**

In `crates/engine/src/serve.rs`, find (inside the `Command::SendPrompt` arm):

```rust
                let controls = TurnControls { approver, pauser };
```

Replace with:

```rust
                let controls = TurnControls {
                    approver,
                    pauser,
                    tools: None,
                    router: None,
                };
```

- [ ] **Step 4: Run the existing test suites to confirm nothing broke**

Run: `cargo test -p otto-engine --lib service:: && cargo test -p otto-engine --test serve`
Expected: all pre-existing tests still pass (this step only touches internal resolution logic and one struct literal; behavior for `SendPrompt`/`run_prompt` is unchanged since both new fields default/pass `None`).

- [ ] **Step 5: Write the two override tests**

Add to `crates/engine/src/service.rs`'s `#[cfg(test)] mod tests` block, after `run_prompt_streams_persists_and_marks_done` (or any convenient spot below the existing helpers):

```rust
    #[tokio::test]
    async fn run_prompt_with_controls_tools_override_restricts_available_tools() {
        let dir = tempfile::tempdir().unwrap();
        let service = service_in(&dir, crate::build_default_registry()).await;
        let id = service
            .create_session("g", &serde_json::json!({}))
            .await
            .unwrap();

        // Narrow to a tool set that excludes fs.write — the Coder's edit-apply check must Deny
        // (ToolRegistry::check denies a tool absent from a subset-narrowed registry).
        let ws: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(dir.path()));
        let base = crate::build_tool_registry(ws, dir.path().to_path_buf());
        let narrowed = Arc::new(base.subset(&["fs.read".to_string()]));

        let controls = TurnControls {
            approver: Arc::new(DenyApprover),
            pauser: Arc::new(NeverPause),
            tools: Some(narrowed),
            router: None,
        };
        let mut sink = CollectingSink::default();
        service
            .run_prompt_with_controls(id, "g", &mut sink, controls)
            .await
            .unwrap();

        // Without fs.write, the Coder's edit was never applied — proving the override, not
        // the service's own (unnarrowed) `self.tools`, is what the orchestrator actually used.
        assert!(
            service
                .workspace()
                .read(std::path::Path::new("out.txt"))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn run_prompt_with_controls_router_override_takes_precedence_over_service_default() {
        let dir = tempfile::tempdir().unwrap();
        // The service's OWN default router (scripted_router()) would write "hi g".
        let service = service_in(&dir, crate::build_default_registry()).await;
        let id = service
            .create_session("g", &serde_json::json!({}))
            .await
            .unwrap();

        // A distinctly different override router.
        let override_provider = ScriptedProvider::new("{}")
            .on(
                "edits",
                r#"{"edits": [{"path": "out.txt", "contents": "OVERRIDDEN"}]}"#,
            )
            .on("milestones", r#"{"milestones": [{"description": "x"}]}"#);
        let override_router: Arc<dyn Router> =
            Arc::new(SingleProviderRouter::new(Arc::new(override_provider)));

        let controls = TurnControls {
            approver: Arc::new(DenyApprover),
            pauser: Arc::new(NeverPause),
            tools: None,
            router: Some(override_router),
        };
        let mut sink = CollectingSink::default();
        service
            .run_prompt_with_controls(id, "g", &mut sink, controls)
            .await
            .unwrap();

        let contents = service
            .workspace()
            .read(std::path::Path::new("out.txt"))
            .await
            .unwrap();
        assert_eq!(contents, b"OVERRIDDEN");
    }
```

- [ ] **Step 6: Run the tests**

Run: `cargo test -p otto-engine --lib service::`
Expected: all pass, including the two new tests.

- [ ] **Step 7: Commit**

```bash
git add crates/engine/src/service.rs crates/engine/src/serve.rs
git commit -m "feat(engine): TurnControls gains per-call tools/router overrides"
```

---

### Task 3: `EngineService::run_command_with_controls`

**Files:**
- Modify: `crates/engine/src/service.rs` (imports, struct field, constructor, new builder method, new method, test module)

- [ ] **Step 1: Add imports**

In `crates/engine/src/service.rs`, add to the `use` block at the top:

```rust
use otto_extensions::{Extensions, expand_args, resolve_injections};
```

- [ ] **Step 2: Add the `extensions` field**

Replace:

```rust
pub struct EngineService {
    store: Arc<dyn SessionStore>,
    registry: Arc<AgentRegistry>,
    router: Arc<dyn Router>,
    workspace: Arc<dyn Workspace>,
    tools: Arc<ToolRegistry>,
    retriever: Option<Arc<dyn Retriever>>,
    turn_lock: tokio::sync::Mutex<()>,
}
```

with:

```rust
pub struct EngineService {
    store: Arc<dyn SessionStore>,
    registry: Arc<AgentRegistry>,
    router: Arc<dyn Router>,
    workspace: Arc<dyn Workspace>,
    tools: Arc<ToolRegistry>,
    retriever: Option<Arc<dyn Retriever>>,
    extensions: Option<Arc<Extensions>>,
    turn_lock: tokio::sync::Mutex<()>,
}
```

- [ ] **Step 3: Initialize the field in `new`, and add the builder**

Replace:

```rust
    pub fn new(
        store: Arc<dyn SessionStore>,
        registry: Arc<AgentRegistry>,
        router: Arc<dyn Router>,
        workspace: Arc<dyn Workspace>,
        tools: Arc<ToolRegistry>,
    ) -> Self {
        Self {
            store,
            registry,
            router,
            workspace,
            tools,
            retriever: None,
            turn_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// Attach a retriever (the indexed candidate source). `None` keeps the lexical fallback.
    pub fn with_retriever(mut self, retriever: Option<Arc<dyn Retriever>>) -> Self {
        self.retriever = retriever;
        self
    }
```

with:

```rust
    pub fn new(
        store: Arc<dyn SessionStore>,
        registry: Arc<AgentRegistry>,
        router: Arc<dyn Router>,
        workspace: Arc<dyn Workspace>,
        tools: Arc<ToolRegistry>,
    ) -> Self {
        Self {
            store,
            registry,
            router,
            workspace,
            tools,
            retriever: None,
            extensions: None,
            turn_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// Attach a retriever (the indexed candidate source). `None` keeps the lexical fallback.
    pub fn with_retriever(mut self, retriever: Option<Arc<dyn Retriever>>) -> Self {
        self.retriever = retriever;
        self
    }

    /// Attach the discovered `.claude/` extensions so `run_command_with_controls` (≙
    /// `Command::RunCommand`) can resolve a command by name. `None` (the default — every
    /// existing `EngineService::new` call site) means every `RunCommand` call fails with a
    /// clear error; there is no silent no-op.
    pub fn with_extensions(mut self, extensions: Arc<Extensions>) -> Self {
        self.extensions = Some(extensions);
        self
    }
```

- [ ] **Step 4: Add `run_command_with_controls`**

Add this new method right after `run_prompt_with_controls` (after its closing `}`, before `validate_workspace_edits`):

```rust
    /// Look up `name` in the discovered custom commands (set via `with_extensions`), expand its
    /// template with `args`, resolve `!bash`/`@file` injections through a tool registry narrowed
    /// to its `allowed-tools`, and run the result as a normal turn with a router pinned to its
    /// `model`. (≙ `Command::RunCommand`.) Errors before any turn starts — unknown `name`, or an
    /// injection failure (e.g. the sensitive-path floor denying `@.env`) — so no `seq` is
    /// consumed and the session is untouched.
    pub async fn run_command_with_controls(
        &self,
        session: SessionId,
        name: &str,
        args: &[String],
        sink: &mut dyn EventSink,
        approver: Arc<dyn Approver>,
        pauser: Arc<dyn PauseController>,
    ) -> anyhow::Result<TurnOutcome> {
        let extensions = self.extensions.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "no command named '{name}': this server was not configured with any extensions"
            )
        })?;
        let def = extensions
            .commands
            .iter()
            .find(|c| c.name == name)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "no command named '{name}' in ~/.claude/commands/ or the project .claude/commands/"
                )
            })?;

        let narrowed_tools: Arc<ToolRegistry> = match &def.allowed_tools {
            Some(list) => Arc::new(self.tools.subset(list)),
            None => Arc::clone(&self.tools),
        };

        let expanded = expand_args(&def.template, args);
        let goal = resolve_injections(&expanded, &narrowed_tools).await?;

        let pinned_router: Arc<dyn Router> =
            Arc::from(crate::build_router_with_model(def.model.as_deref()));

        let controls = TurnControls {
            approver,
            pauser,
            tools: Some(narrowed_tools),
            router: Some(pinned_router),
        };
        self.run_prompt_with_controls(session, &goal, sink, controls)
            .await
    }
```

- [ ] **Step 5: Write the unit tests**

Add to the `#[cfg(test)] mod tests` block:

```rust
    fn command_def(
        name: &str,
        template: &str,
        allowed_tools: Option<Vec<String>>,
    ) -> otto_extensions::CustomCommandDef {
        otto_extensions::CustomCommandDef {
            name: name.to_string(),
            description: None,
            argument_hint: None,
            model: None,
            allowed_tools,
            template: template.to_string(),
        }
    }

    #[tokio::test]
    async fn run_command_with_controls_unknown_name_errors_without_starting_a_turn() {
        let dir = tempfile::tempdir().unwrap();
        let service = service_in(&dir, crate::build_default_registry())
            .await
            .with_extensions(Arc::new(Extensions::default()));
        let id = service
            .create_session("g", &serde_json::json!({}))
            .await
            .unwrap();

        let mut sink = CollectingSink::default();
        let err = service
            .run_command_with_controls(
                id,
                "nope",
                &[],
                &mut sink,
                Arc::new(DenyApprover),
                Arc::new(NeverPause),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no command named 'nope'"));

        let replayed = service.store().replay_since(id, None).await.unwrap();
        assert!(replayed.is_empty(), "no turn should have started");
    }

    #[tokio::test]
    async fn run_command_with_controls_expands_args_and_narrows_tools() {
        let dir = tempfile::tempdir().unwrap();
        let def = command_def("greet", "do $1", Some(vec!["fs.read".to_string()]));
        let extensions = Extensions {
            commands: vec![def],
            ..Default::default()
        };
        let service = service_in(&dir, crate::build_default_registry())
            .await
            .with_extensions(Arc::new(extensions));
        let id = service
            .create_session("g", &serde_json::json!({}))
            .await
            .unwrap();

        let mut sink = CollectingSink::default();
        let outcome = service
            .run_command_with_controls(
                id,
                "greet",
                &["thing".to_string()],
                &mut sink,
                Arc::new(DenyApprover),
                Arc::new(NeverPause),
            )
            .await
            .unwrap();
        assert!(outcome.ok);

        // Expansion: the recorded turn's goal is the expanded template, not the raw one.
        let state = service.store().snapshot(id).await.unwrap();
        assert_eq!(state.turns.last().unwrap().goal, "do thing");

        // Narrowing: fs.write was excluded, so the Coder's edit was never applied.
        assert!(
            service
                .workspace()
                .read(std::path::Path::new("out.txt"))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn run_command_with_controls_injection_failure_errors_without_starting_a_turn() {
        let dir = tempfile::tempdir().unwrap();
        let def = command_def("leak", "secret: @.env", None);
        let extensions = Extensions {
            commands: vec![def],
            ..Default::default()
        };
        let service = service_in(&dir, crate::build_default_registry())
            .await
            .with_extensions(Arc::new(extensions));
        let id = service
            .create_session("g", &serde_json::json!({}))
            .await
            .unwrap();

        let mut sink = CollectingSink::default();
        let result = service
            .run_command_with_controls(
                id,
                "leak",
                &[],
                &mut sink,
                Arc::new(DenyApprover),
                Arc::new(NeverPause),
            )
            .await;
        assert!(
            result.is_err(),
            "the sensitive-path floor must fail the @.env injection closed"
        );

        let replayed = service.store().replay_since(id, None).await.unwrap();
        assert!(replayed.is_empty(), "no turn should have started");
    }
```

- [ ] **Step 6: Run the tests**

Run: `cargo test -p otto-engine --lib service::`
Expected: all pass, including the three new `run_command_with_controls_*` tests.

- [ ] **Step 7: Commit**

```bash
git add crates/engine/src/service.rs
git commit -m "feat(engine): EngineService::run_command_with_controls"
```

---

### Task 4: `serve.rs` — factor the turn-loop race into a shared helper (no behavior change)

**Files:**
- Modify: `crates/engine/src/serve.rs:1-35` (imports)
- Modify: `crates/engine/src/serve.rs:292-294` (near `WsWriter` — add `WsReader` alias)
- Modify: `crates/engine/src/serve.rs:499-573` (the `Command::SendPrompt` arm)

This task is a pure refactor: it must not change observable behavior. It exists so Task 5 can add `Command::RunCommand` without duplicating the ~70-line pause/resume/approval/abort racing logic.

- [ ] **Step 1: Add the two missing imports**

In `crates/engine/src/serve.rs`, replace:

```rust
use futures_util::SinkExt;
use futures_util::stream::{SplitSink, StreamExt};
```

with:

```rust
use futures_util::SinkExt;
use futures_util::stream::{SplitSink, SplitStream, StreamExt};
use otto_engine_core::TurnOutcome;
```

- [ ] **Step 2: Add the `WsReader` alias next to `WsWriter`**

Find:

```rust
/// The writer half of a split WebSocket — what events and frames are sent through.
type WsWriter = SplitSink<WebSocket, Message>;
```

Add right after it:

```rust
/// The reader half of a split WebSocket — inbound frames are read through this.
type WsReader = SplitStream<WebSocket>;
```

- [ ] **Step 3: Add `TurnLoopOutcome` and `run_turn_loop`**

Add this right before `async fn handle_socket(...)`:

```rust
/// The result of racing a turn future against inbound socket frames.
enum TurnLoopOutcome {
    /// The turn future resolved (successfully or with an error). The connection stays open.
    Finished(Option<anyhow::Error>),
    /// An explicit `Abort` or a disconnect ended things — the caller must stop reading frames
    /// entirely.
    StopOuterLoop,
}

/// Drive `turn` to completion while concurrently reading inbound frames for
/// `ApproveDiff`/`Pause`/`Resume`/`Abort`. Shared by every command that starts a turn
/// (`SendPrompt`, `RunCommand`) so their concurrency behavior can never drift apart.
async fn run_turn_loop(
    turn: impl std::future::Future<Output = anyhow::Result<TurnOutcome>>,
    reader: &mut WsReader,
    approvals: &ApprovalRegistry,
    pause_state: &PauseState,
    state: &ServeState,
    session: SessionId,
) -> TurnLoopOutcome {
    tokio::pin!(turn);
    loop {
        tokio::select! {
            res = &mut turn => {
                let err = res.err();
                approvals.clear();
                // Drop any leftover pause flag so a Pause that arrived but was never resumed
                // before the turn ended cannot pre-pause the next one.
                pause_state.resume_all();
                return TurnLoopOutcome::Finished(err);
            }
            inbound = reader.next() => match inbound {
                Some(Ok(Message::Text(t))) => {
                    match serde_json::from_str::<Command>(t.as_str()) {
                        Ok(Command::ApproveDiff { id, approved, .. }) => {
                            approvals.resolve(id, approved);
                        }
                        Ok(Command::Pause { .. }) => {
                            pause_state.pause();
                        }
                        Ok(Command::Resume { .. }) => {
                            pause_state.resume_all();
                        }
                        Ok(Command::Abort { .. }) => {
                            let _ = state.service.abort(session).await;
                            approvals.clear();
                            pause_state.resume_all();
                            return TurnLoopOutcome::StopOuterLoop;
                        }
                        // A second SendPrompt/RunCommand mid-turn is ignored (one turn at a
                        // time); other commands are no-ops here.
                        _ => {}
                    }
                }
                Some(Ok(Message::Close(_))) | None => {
                    approvals.clear();
                    pause_state.resume_all();
                    return TurnLoopOutcome::StopOuterLoop;
                }
                _ => {}
            }
        }
    }
}
```

- [ ] **Step 4: Rewrite the `SendPrompt` arm to use the helper**

Replace the whole `Command::SendPrompt { text, .. } => { ... }` arm (currently lines 500-573) with:

```rust
            Command::SendPrompt { text, .. } => {
                let approver = Arc::new(InteractiveApprover::new(approvals.clone()));
                let pauser = Arc::new(InteractivePauser(Arc::clone(&pause_state)));
                let controls = TurnControls {
                    approver,
                    pauser,
                    tools: None,
                    router: None,
                };
                // Drive the turn while concurrently reading inbound approvals. The turn borrows
                // `writer` (via the sink); `run_turn_loop` borrows `reader` — disjoint, so it can
                // poll both.
                let outcome = {
                    let mut sink = WsSink {
                        writer: &mut writer,
                    };
                    let turn = state
                        .service
                        .run_prompt_with_controls(session, &text, &mut sink, controls);
                    run_turn_loop(turn, &mut reader, &approvals, &pause_state, &state, session).await
                }; // `sink` dropped here → `writer` is free again

                match outcome {
                    TurnLoopOutcome::Finished(Some(e)) => {
                        let _ = send_msg(
                            &mut writer,
                            &ServerMessage::Error {
                                message: e.to_string(),
                            },
                        )
                        .await;
                    }
                    TurnLoopOutcome::Finished(None) => {}
                    TurnLoopOutcome::StopOuterLoop => break 'outer,
                }
            }
```

- [ ] **Step 5: Run the existing serve test suite to confirm the refactor is behavior-preserving**

Run: `cargo test -p otto-engine --test serve`
Expected: every existing test still passes (pause/resume, approval, abort, reconnect/replay, token-cost-meter streaming) — this step only restructures the `SendPrompt` arm, it does not change any behavior.

- [ ] **Step 6: Commit**

```bash
git add crates/engine/src/serve.rs
git commit -m "refactor(engine): factor serve.rs's turn-loop racing into run_turn_loop"
```

---

### Task 5: `serve.rs` — wire `Command::RunCommand`

**Files:**
- Modify: `crates/engine/src/serve.rs` (the command `match` in `handle_socket`)

- [ ] **Step 1: Add the new arm**

In `handle_socket`'s `match command { ... }`, add a new arm right after the `Command::SendPrompt` arm (before `Command::Abort`):

```rust
            Command::RunCommand { name, args, .. } => {
                let approver = Arc::new(InteractiveApprover::new(approvals.clone()));
                let pauser = Arc::new(InteractivePauser(Arc::clone(&pause_state)));
                let outcome = {
                    let mut sink = WsSink {
                        writer: &mut writer,
                    };
                    let turn = state.service.run_command_with_controls(
                        session, &name, &args, &mut sink, approver, pauser,
                    );
                    run_turn_loop(turn, &mut reader, &approvals, &pause_state, &state, session).await
                }; // `sink` dropped here → `writer` is free again

                match outcome {
                    TurnLoopOutcome::Finished(Some(e)) => {
                        let _ = send_msg(
                            &mut writer,
                            &ServerMessage::Error {
                                message: e.to_string(),
                            },
                        )
                        .await;
                    }
                    TurnLoopOutcome::Finished(None) => {}
                    TurnLoopOutcome::StopOuterLoop => break 'outer,
                }
            }
```

- [ ] **Step 2: Build**

Run: `cargo build -p otto-engine`
Expected: clean build (the `Command` enum's new `RunCommand` variant is now handled everywhere it's matched).

- [ ] **Step 3: Commit**

```bash
git add crates/engine/src/serve.rs
git commit -m "feat(engine): wire Command::RunCommand into the serve WebSocket handler"
```

---

### Task 6: `main.rs` — attach extensions to the served `EngineService`

**Files:**
- Modify: `crates/engine/src/main.rs` (inside `cmd_serve`, around the `EngineService::new(...)` construction)

- [ ] **Step 1: Chain `.with_extensions(...)`**

In `crates/engine/src/main.rs`, find (inside `cmd_serve`):

```rust
    let service = otto_engine::EngineService::new(store, registry, router, orch_workspace, tools)
        .with_retriever(retriever);
```

Replace with:

```rust
    let service = otto_engine::EngineService::new(store, registry, router, orch_workspace, tools)
        .with_retriever(retriever)
        .with_extensions(Arc::new(ext));
```

(`ext` is the `otto_extensions::Extensions` already discovered earlier in `cmd_serve` and passed by reference into `build_serve_tools(&ext, ...)` — it is not consumed there, so it is still available here by value.)

- [ ] **Step 2: Build**

Run: `cargo build -p otto-engine`
Expected: clean build.

- [ ] **Step 3: Run the full `otto-engine` test suite**

Run: `cargo test -p otto-engine`
Expected: all tests pass (this is the first point where every prior task's code is exercised together).

- [ ] **Step 4: Commit**

```bash
git add crates/engine/src/main.rs
git commit -m "feat(engine): otto serve attaches discovered extensions to its EngineService"
```

---

### Task 7: Integration test — `Command::RunCommand` over a real socket

**Files:**
- Modify: `crates/engine/tests/serve.rs` (new imports, one new `start_*` helper, two new tests)

- [ ] **Step 1: Add the imports this test needs**

In `crates/engine/tests/serve.rs`, replace:

```rust
use otto_engine::{
    EngineService, build_default_registry, build_tool_registry, build_tool_registry_approving,
    serve_app, serve_run,
};
```

with:

```rust
use otto_engine::{
    EngineService, build_default_registry, build_tool_registry, build_tool_registry_approving,
    serve_app, serve_run,
};
use otto_extensions::{CustomCommandDef, Extensions};
```

- [ ] **Step 2: Add a server-starter with a fixture command wired in**

Add this new function near the other `start_*` helpers (e.g. right after `start_server`):

```rust
/// Start the serve app with one discovered command: `greet`, template `hi $1`, narrowed to
/// `fs.read` only (excludes `fs.write` — proves narrowing reaches a served RunCommand turn).
/// The router's ScriptedProvider ignores the exact goal text, so any expansion is acceptable;
/// what's asserted is the narrowing (no file write) and the recorded per-turn goal.
async fn start_server_with_command() -> (u16, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let provider = ScriptedProvider::new("{}")
        .on(
            "edits",
            r#"{"edits": [{"path": "out.txt", "contents": "should not land"}]}"#,
        )
        .on("milestones", r#"{"milestones": [{"description": "x"}]}"#);
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
    let extensions = Extensions {
        commands: vec![CustomCommandDef {
            name: "greet".to_string(),
            description: None,
            argument_hint: None,
            model: None,
            allowed_tools: Some(vec!["fs.read".to_string()]),
            template: "hi $1".to_string(),
        }],
        ..Default::default()
    };
    let service = EngineService::new(
        store,
        Arc::new(build_default_registry()),
        router,
        workspace,
        tools,
    )
    .with_extensions(Arc::new(extensions));

    let app = serve_app(service, TOKEN.to_string(), test_capabilities(), None, false);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        serve_run(listener, app, None).await.unwrap();
    });
    (port, dir)
}
```

- [ ] **Step 3: Write the two tests**

Add these tests anywhere in the file (e.g. right after `streams_a_turn_then_reconnects_with_replay`):

```rust
#[tokio::test]
async fn run_command_expands_and_narrows_tools() {
    let (port, dir) = start_server_with_command().await;
    let (mut ws, _) = tokio_tungstenite::connect_async(authed_request(port, ""))
        .await
        .expect("connect");
    let ready: Value = next_json(&mut ws).await;
    let session = ready["session"].as_str().unwrap().to_string();

    let cmd = serde_json::json!({
        "RunCommand": { "session": session, "name": "greet", "args": ["world"] }
    });
    ws.send(Message::Text(serde_json::to_string(&cmd).unwrap()))
        .await
        .unwrap();

    let mut completed = false;
    while let Some(frame) = next_json_opt(&mut ws).await {
        if frame["type"] == "event" && frame["event"]["kind"].get("TurnComplete").is_some() {
            completed = true;
            break;
        }
    }
    assert!(completed, "expected the RunCommand turn to complete");

    // Narrowing worked: fs.write was excluded from the command's tools, so the Coder's edit
    // (which the scripted provider always proposes) was never applied.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(
        !dir.path().join("out.txt").exists(),
        "allowed-tools must have excluded fs.write"
    );
}

#[tokio::test]
async fn run_command_unknown_name_reports_error_and_keeps_connection_open() {
    let (port, _dir) = start_server_with_command().await;
    let (mut ws, _) = tokio_tungstenite::connect_async(authed_request(port, ""))
        .await
        .expect("connect");
    let ready: Value = next_json(&mut ws).await;
    let session = ready["session"].as_str().unwrap().to_string();

    let cmd = serde_json::json!({
        "RunCommand": { "session": session.clone(), "name": "nope", "args": [] }
    });
    ws.send(Message::Text(serde_json::to_string(&cmd).unwrap()))
        .await
        .unwrap();

    let frame = next_json(&mut ws).await;
    assert_eq!(frame["type"], "error");
    assert!(
        frame["message"].as_str().unwrap().contains("no command named 'nope'"),
        "got: {frame}"
    );

    // The connection is still usable afterward.
    let cmd2 = serde_json::json!({ "SendPrompt": { "session": session, "text": "add a greeting" } });
    ws.send(Message::Text(serde_json::to_string(&cmd2).unwrap()))
        .await
        .unwrap();
    let mut completed = false;
    while let Some(frame) = next_json_opt(&mut ws).await {
        if frame["type"] == "event" && frame["event"]["kind"].get("TurnComplete").is_some() {
            completed = true;
            break;
        }
    }
    assert!(completed, "connection must survive an unknown RunCommand");
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p otto-engine --test serve`
Expected: all pass, including the two new tests.

- [ ] **Step 5: Commit**

```bash
git add crates/engine/tests/serve.rs
git commit -m "test(engine): RunCommand integration coverage over a real socket"
```

---

### Task 8: Docs + full regression

**Files:**
- Modify: `CLAUDE.md` (the `extensions` row in the crate table)
- No other files (verification only)

- [ ] **Step 1: Update the `extensions` row**

In `CLAUDE.md`, find this sentence at the very end of the `extensions` row (currently the last sentence before the row's closing ` |`):

```
An unreachable plugin server is logged and skipped, never fatal. The `--agent`/`--command` subpaths remain the only deferred serve-path thread. |
```

Replace it with:

```
An unreachable plugin server is logged and skipped, never fatal. Slice 12 ships **`Command::RunCommand` on `otto serve`**: `EngineService` gains a `with_extensions` builder (the same `Extensions` `cmd_serve` already discovers for `build_serve_tools`) and a `run_command_with_controls` method — look up the command by name, narrow the tool registry via `ToolRegistry::subset(allowed_tools)`, `expand_args` + `resolve_injections`, pin the router via `build_router_with_model(model)`, then run the result as an ordinary turn through `run_prompt_with_controls` (which now accepts optional per-call `tools`/`router` overrides on `TurnControls`, defaulting to the service's own so `SendPrompt` is unaffected). Because it narrows the *already-composed* serve tool registry (permissions/hooks/skills/plugins, per Slices 6–11) rather than the CLI path's `PermissionRules::default()`, a served `RunCommand` turn is strictly more correct than `otto run --command` today — an intentional, serve-only asymmetry. `--agent` on serve remains the only deferred serve-path thread (a separate plan: it bypasses the orchestrator turn machinery entirely, via `TaskTool`/`MarkdownAgent`, and needs its own event-synthesis design). |
```

- [ ] **Step 2: Build the whole workspace**

Run: `cargo build --workspace`
Expected: clean build, no errors or new warnings.

- [ ] **Step 3: Run the full offline test suite**

Run: `cargo test --workspace`
Expected: all tests pass (fully offline/deterministic, no network or API keys involved).

- [ ] **Step 4: Format check**

Run: `cargo fmt --all -- --check`
Expected: no diff.

- [ ] **Step 5: Lint**

Run: `cargo clippy --workspace --all-targets`
Expected: no warnings introduced by this change.

- [ ] **Step 6: Commit**

```bash
git add CLAUDE.md
git commit -m "docs: record extensions Slice 12 (Command::RunCommand on otto serve)"
```

---

## Plan coverage check

- New `Command::RunCommand` protocol variant, round-trip tested → Task 1.
- `TurnControls` per-call `tools`/`router` overrides, proven to actually take effect → Task 2.
- `EngineService::run_command_with_controls` (lookup, narrow, expand, resolve injections, pin router, run) → Task 3.
- Serve-only correctness improvement over the CLI (narrowing the composed registry, not an empty one) → Task 3 Step 4 (narrowing test uses `service_in`'s full `build_tool_registry`, not `PermissionRules::default()`), called out again in Task 8's doc update.
- `serve.rs` dispatches `Command::RunCommand` identically to `SendPrompt` (same pause/resume/approval/abort race) → Task 4 (shared helper, regression-verified) + Task 5 (new arm).
- Extensions discovered once at server startup, no re-discovery per connection → Task 6.
- Integration coverage over a real socket: narrowing + expansion, and unknown-name error keeping the connection alive → Task 7.
- Docs reflect the shipped state and the remaining `--agent` gap → Task 8.
- Full-workspace regression (build/test/fmt/clippy) → Task 8.
- Explicitly out of scope (per the design spec and the brainstorming decision to split into two sub-projects): `--agent` on serve, hot-reloading `.claude/commands/*.md` while serving, and any change to `otto run --command`'s existing CLI behavior.
