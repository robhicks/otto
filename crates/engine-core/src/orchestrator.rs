//! The deterministic orchestrator spine: Plan -> Execute -> Verify -> Done.
//! It owns control flow and event emission; capabilities live in the agents.

use otto_protocol::{EventKind, Role, SessionId};
use uuid::Uuid;

use crate::registry::AgentRegistry;
use crate::router::Router;
use crate::tool::{Approver, Decision, ToolRegistry};
use crate::traits::{AgentCtx, Workspace};
use crate::types::{AgentOutput, AgentRequest};

/// Sink for engine -> frontend events. The engine supplies a real implementation;
/// tests supply a collecting closure.
pub trait Emitter: Send + Sync {
    fn emit(&self, kind: EventKind);
}

impl<F: Fn(EventKind) + Send + Sync> Emitter for F {
    fn emit(&self, kind: EventKind) {
        self(kind)
    }
}

/// The result of running a single turn.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnOutcome {
    pub ok: bool,
}

pub struct Orchestrator<'a> {
    pub registry: &'a AgentRegistry,
    pub router: &'a dyn Router,
    pub workspace: &'a dyn Workspace,
    pub tools: &'a ToolRegistry,
    /// Resolves an `Ask` verdict on a proposed edit to apply/skip (interactive approval).
    pub approver: &'a dyn Approver,
    /// Mints the correlation id for an `ApprovalRequest`. Injected by the engine layer so the
    /// orchestrator stays free of nondeterministic calls (the offline path never reaches it).
    pub next_id: &'a (dyn Fn() -> Uuid + Send + Sync),
    /// Running token totals for this turn (fed by the engine's `MeteringRouter`). The
    /// orchestrator reads it to emit `TokenCostMeter`; zero totals (offline) emit nothing.
    pub meter: &'a crate::meter::TokenMeter,
    /// Cooperative pause checked at phase boundaries (wired in the pause task).
    pub pauser: &'a dyn crate::tool::PauseController,
}

impl<'a> Orchestrator<'a> {
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

    /// Run a single orchestrator turn for `goal`: Plan -> Execute -> Verify -> Done.
    /// Events are emitted in deterministic order via `emit`. Session sequencing
    /// (monotonic `seq` on `Event`) is applied by the engine layer, not here.
    pub async fn run_turn(
        &self,
        _session: SessionId,
        goal: &str,
        emit: &dyn Emitter,
    ) -> anyhow::Result<TurnOutcome> {
        let ctx = AgentCtx::new(self.router, self.workspace, self.tools);

        // --- Plan ---
        self.checkpoint(emit).await;
        emit.emit(EventKind::AgentStarted {
            role: Role::Planner,
        });
        let plan = self
            .registry
            .get(&Role::Planner)?
            .run(
                AgentRequest::Plan {
                    goal: goal.to_string(),
                },
                &ctx,
            )
            .await?;
        let AgentOutput::Plan { milestones } = plan else {
            anyhow::bail!("planner returned unexpected output");
        };
        emit.emit(EventKind::Log {
            message: format!("planned {} milestone(s)", milestones.len()),
        });
        emit.emit(EventKind::AgentFinished {
            role: Role::Planner,
        });
        self.emit_meter(emit);

        // --- Execute ---
        self.checkpoint(emit).await;
        emit.emit(EventKind::AgentStarted {
            role: Role::ContextFinder,
        });
        let context = self
            .registry
            .get(&Role::ContextFinder)?
            .run(
                AgentRequest::FindContext {
                    goal: goal.to_string(),
                },
                &ctx,
            )
            .await?;
        let AgentOutput::Context { files } = context else {
            anyhow::bail!("context finder returned unexpected output");
        };
        emit.emit(EventKind::AgentFinished {
            role: Role::ContextFinder,
        });
        self.emit_meter(emit);

        // --- Code -> apply (gated) -> Verify -> (Repair) ---
        // On verify failure, re-run the Coder with the failure as feedback and re-verify,
        // up to MAX_REPAIRS attempts, escalating routing via prior_failures each time.
        const MAX_REPAIRS: u32 = 2;
        let mut prior_failures: u32 = 0;
        let mut feedback: Option<String> = None;

        let ok = loop {
            // Coder
            self.checkpoint(emit).await;
            emit.emit(EventKind::AgentStarted { role: Role::Coder });
            let coded = self
                .registry
                .get(&Role::Coder)?
                .run(
                    AgentRequest::Code {
                        goal: goal.to_string(),
                        milestones: milestones.clone(),
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
                match self.tools.check("fs.write", &check_args) {
                    Decision::Allow => {}
                    Decision::Deny => {
                        emit.emit(EventKind::Log {
                            message: format!(
                                "edit to {} denied by permission gate; skipped",
                                edit.path.display()
                            ),
                        });
                        continue;
                    }
                    Decision::Ask => {
                        // Read current contents for the diff (None if the file does not exist).
                        let old = self
                            .workspace
                            .read(&edit.path)
                            .await
                            .ok()
                            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned());
                        let id = (self.next_id)();
                        emit.emit(EventKind::ApprovalRequest {
                            id,
                            path: edit.path.clone(),
                            old: old.clone(),
                            new: edit.new_contents.clone(),
                        });
                        let approved = self
                            .approver
                            .request(id, &edit.path, old.as_deref(), &edit.new_contents)
                            .await;
                        if !approved {
                            emit.emit(EventKind::Log {
                                message: format!(
                                    "edit to {} rejected by approver; skipped",
                                    edit.path.display()
                                ),
                            });
                            continue;
                        }
                    }
                }
                let bytes_written = self.workspace.apply_edit(edit).await?;
                emit.emit(EventKind::FileEdit {
                    path: edit.path.clone(),
                    bytes_written,
                });
            }
            emit.emit(EventKind::AgentFinished { role: Role::Coder });
            self.emit_meter(emit);

            // Verify
            self.checkpoint(emit).await;
            emit.emit(EventKind::AgentStarted {
                role: Role::Verifier,
            });
            let verified = self
                .registry
                .get(&Role::Verifier)?
                .run(AgentRequest::Verify, &ctx)
                .await?;
            let AgentOutput::Verify { ok, detail } = verified else {
                anyhow::bail!("verifier returned unexpected output");
            };
            emit.emit(EventKind::VerifyResult {
                ok,
                detail: detail.clone(),
            });
            emit.emit(EventKind::AgentFinished {
                role: Role::Verifier,
            });
            self.emit_meter(emit);

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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meter::TokenMeter;
    use crate::router::{RouteHints, Router};
    use crate::tool::{
        Approver, Decision, DenyApprover, DenyAsk, NeverPause, PermissionGate, ToolRegistry,
    };
    use crate::traits::{Agent, Workspace, WorkspaceRead};
    use crate::types::{
        CompleteRequest, CompleteResponse, Edit, Milestone, Usage, WorkspaceSnapshot,
    };
    use async_trait::async_trait;
    use serde_json::Value;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};
    use uuid::Uuid;

    /// A fixed id so `ApprovalRequest` assertions are deterministic in tests.
    fn test_id() -> Uuid {
        Uuid::from_u128(0)
    }

    struct TestAllowGate;
    impl PermissionGate for TestAllowGate {
        fn evaluate(&self, _tool: &str, _args: &Value) -> Decision {
            Decision::Allow
        }
    }
    fn empty_tools() -> ToolRegistry {
        ToolRegistry::new(Arc::new(TestAllowGate), Arc::new(DenyAsk))
    }

    struct TestDenyWriteGate;
    impl PermissionGate for TestDenyWriteGate {
        fn evaluate(&self, tool: &str, _args: &Value) -> Decision {
            if tool == "fs.write" {
                Decision::Deny
            } else {
                Decision::Allow
            }
        }
    }
    fn deny_write_tools() -> ToolRegistry {
        ToolRegistry::new(Arc::new(TestDenyWriteGate), Arc::new(DenyAsk))
    }

    struct TestAskWriteGate;
    impl PermissionGate for TestAskWriteGate {
        fn evaluate(&self, tool: &str, _args: &Value) -> Decision {
            if tool == "fs.write" {
                Decision::Ask
            } else {
                Decision::Allow
            }
        }
    }

    struct FakeRouter;
    #[async_trait]
    impl Router for FakeRouter {
        async fn complete(
            &self,
            _req: CompleteRequest,
            _hints: RouteHints,
        ) -> anyhow::Result<CompleteResponse> {
            Ok(CompleteResponse {
                text: "fake".to_string(),
                usage: None,
            })
        }
    }

    #[derive(Default)]
    struct RecordingWorkspace {
        edits: Mutex<Vec<Edit>>,
    }
    #[async_trait]
    impl WorkspaceRead for RecordingWorkspace {
        async fn read(&self, _path: &Path) -> anyhow::Result<Vec<u8>> {
            Ok(Vec::new())
        }
        async fn list(&self, _glob: &str) -> anyhow::Result<Vec<PathBuf>> {
            Ok(Vec::new())
        }
    }
    #[async_trait]
    impl Workspace for RecordingWorkspace {
        async fn apply_edit(&self, edit: &Edit) -> anyhow::Result<u64> {
            self.edits.lock().unwrap().push(edit.clone());
            Ok(edit.new_contents.len() as u64)
        }
        async fn snapshot(&self) -> anyhow::Result<WorkspaceSnapshot> {
            Ok(WorkspaceSnapshot { files: Vec::new() })
        }
    }

    struct FixedPlanner;
    #[async_trait]
    impl Agent for FixedPlanner {
        async fn run(&self, _req: AgentRequest, _ctx: &AgentCtx) -> anyhow::Result<AgentOutput> {
            Ok(AgentOutput::Plan {
                milestones: vec![Milestone {
                    description: "m".into(),
                }],
            })
        }
    }
    struct EmptyContextFinder;
    #[async_trait]
    impl Agent for EmptyContextFinder {
        async fn run(&self, _req: AgentRequest, _ctx: &AgentCtx) -> anyhow::Result<AgentOutput> {
            Ok(AgentOutput::Context { files: Vec::new() })
        }
    }
    struct OneEditCoder;
    #[async_trait]
    impl Agent for OneEditCoder {
        async fn run(&self, _req: AgentRequest, _ctx: &AgentCtx) -> anyhow::Result<AgentOutput> {
            Ok(AgentOutput::Code {
                edits: vec![Edit {
                    path: PathBuf::from("out.txt"),
                    new_contents: "hi".into(),
                }],
            })
        }
    }
    struct OkVerifier;
    #[async_trait]
    impl Agent for OkVerifier {
        async fn run(&self, _req: AgentRequest, _ctx: &AgentCtx) -> anyhow::Result<AgentOutput> {
            Ok(AgentOutput::Verify {
                ok: true,
                detail: "ok".into(),
            })
        }
    }

    fn registry() -> AgentRegistry {
        let mut r = AgentRegistry::new();
        r.register(Role::Planner, Arc::new(FixedPlanner));
        r.register(Role::ContextFinder, Arc::new(EmptyContextFinder));
        r.register(Role::Coder, Arc::new(OneEditCoder));
        r.register(Role::Verifier, Arc::new(OkVerifier));
        r
    }

    #[tokio::test]
    async fn run_turn_drives_full_spine_and_emits_ordered_events() {
        let reg = registry();
        let router = FakeRouter;
        let workspace = RecordingWorkspace::default();
        let tools = empty_tools();
        let meter = TokenMeter::default();
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

        let outcome = orch
            .run_turn(SessionId::new(), "do the thing", &sink)
            .await
            .unwrap();

        assert_eq!(outcome, TurnOutcome { ok: true });
        assert_eq!(workspace.edits.lock().unwrap().len(), 1);

        let recorded = events.lock().unwrap().clone();
        let expected = vec![
            EventKind::AgentStarted {
                role: Role::Planner,
            },
            EventKind::Log {
                message: "planned 1 milestone(s)".to_string(),
            },
            EventKind::AgentFinished {
                role: Role::Planner,
            },
            EventKind::AgentStarted {
                role: Role::ContextFinder,
            },
            EventKind::AgentFinished {
                role: Role::ContextFinder,
            },
            EventKind::AgentStarted { role: Role::Coder },
            EventKind::FileEdit {
                path: PathBuf::from("out.txt"),
                bytes_written: 2,
            },
            EventKind::AgentFinished { role: Role::Coder },
            EventKind::AgentStarted {
                role: Role::Verifier,
            },
            EventKind::VerifyResult {
                ok: true,
                detail: "ok".to_string(),
            },
            EventKind::AgentFinished {
                role: Role::Verifier,
            },
            EventKind::TurnComplete { ok: true },
        ];
        assert_eq!(recorded, expected);
    }

    #[tokio::test]
    async fn run_turn_errors_when_a_role_is_missing() {
        let mut reg = AgentRegistry::new();
        reg.register(Role::Planner, Arc::new(FixedPlanner));
        let router = FakeRouter;
        let workspace = RecordingWorkspace::default();
        let tools = empty_tools();
        let meter = TokenMeter::default();
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

        let err = orch
            .run_turn(SessionId::new(), "x", &(|_k: EventKind| {}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no agent registered"));
    }

    #[tokio::test]
    async fn denied_edit_is_skipped_and_logged() {
        let reg = registry(); // OneEditCoder produces an edit to out.txt
        let router = FakeRouter;
        let workspace = RecordingWorkspace::default();
        let tools = deny_write_tools();
        let meter = TokenMeter::default();
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

        let outcome = orch.run_turn(SessionId::new(), "x", &sink).await.unwrap();

        assert_eq!(outcome, TurnOutcome { ok: true });
        assert_eq!(workspace.edits.lock().unwrap().len(), 0);

        let recorded = events.lock().unwrap().clone();
        assert!(recorded.iter().any(|e| matches!(
            e,
            EventKind::Log { message } if message.contains("denied by permission gate")
        )));
        assert!(
            !recorded
                .iter()
                .any(|e| matches!(e, EventKind::FileEdit { .. }))
        );
    }

    #[tokio::test]
    async fn ask_verdict_also_skips_edit_fail_closed() {
        let reg = registry(); // OneEditCoder produces an edit to out.txt
        let router = FakeRouter;
        let workspace = RecordingWorkspace::default();
        let tools = ToolRegistry::new(Arc::new(TestAskWriteGate), Arc::new(DenyAsk));
        let meter = TokenMeter::default();
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

        let outcome = orch
            .run_turn(SessionId::new(), "x", &(|_k: EventKind| {}))
            .await
            .unwrap();

        assert_eq!(outcome, TurnOutcome { ok: true });
        // An Ask verdict (not Allow) must NOT apply the edit — fail-closed.
        assert_eq!(workspace.edits.lock().unwrap().len(), 0);
    }

    /// Fails verification `fails_remaining` times, then succeeds.
    struct FlakyVerifier {
        fails_remaining: AtomicU32,
    }
    #[async_trait]
    impl Agent for FlakyVerifier {
        async fn run(&self, _req: AgentRequest, _ctx: &AgentCtx) -> anyhow::Result<AgentOutput> {
            let remaining = self.fails_remaining.load(Ordering::SeqCst);
            if remaining > 0 {
                self.fails_remaining.store(remaining - 1, Ordering::SeqCst);
                Ok(AgentOutput::Verify {
                    ok: false,
                    detail: "boom".into(),
                })
            } else {
                Ok(AgentOutput::Verify {
                    ok: true,
                    detail: "ok".into(),
                })
            }
        }
    }

    struct AlwaysFailVerifier;
    #[async_trait]
    impl Agent for AlwaysFailVerifier {
        async fn run(&self, _req: AgentRequest, _ctx: &AgentCtx) -> anyhow::Result<AgentOutput> {
            Ok(AgentOutput::Verify {
                ok: false,
                detail: "still broken".into(),
            })
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

    #[tokio::test]
    async fn flaky_verifier_triggers_repair_then_succeeds() {
        let reg = registry_with_verifier(Arc::new(FlakyVerifier {
            fails_remaining: AtomicU32::new(1),
        }));
        let router = FakeRouter;
        let workspace = RecordingWorkspace::default();
        let tools = empty_tools();
        let meter = TokenMeter::default();
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

        let outcome = orch.run_turn(SessionId::new(), "x", &sink).await.unwrap();
        assert_eq!(outcome, TurnOutcome { ok: true });

        let recorded = events.lock().unwrap().clone();
        let coder_starts = recorded
            .iter()
            .filter(|e| matches!(e, EventKind::AgentStarted { role: Role::Coder }))
            .count();
        assert_eq!(coder_starts, 2);
        assert!(
            recorded
                .iter()
                .any(|e| matches!(e, EventKind::Log { message } if message.contains("repairing")))
        );
        assert!(recorded.contains(&EventKind::VerifyResult {
            ok: false,
            detail: "boom".into()
        }));
        assert_eq!(recorded.last(), Some(&EventKind::TurnComplete { ok: true }));
    }

    #[tokio::test]
    async fn repair_exhaustion_fails_the_turn() {
        let reg = registry_with_verifier(Arc::new(AlwaysFailVerifier));
        let router = FakeRouter;
        let workspace = RecordingWorkspace::default();
        let tools = empty_tools();
        let meter = TokenMeter::default();
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

        let outcome = orch.run_turn(SessionId::new(), "x", &sink).await.unwrap();
        assert_eq!(outcome, TurnOutcome { ok: false });

        let recorded = events.lock().unwrap().clone();
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
        assert_eq!(
            recorded.last(),
            Some(&EventKind::TurnComplete { ok: false })
        );
    }

    type SeenEntry = (Uuid, PathBuf, Option<String>, String);

    /// Records each approval request and returns a fixed verdict.
    struct ScriptedApprover {
        approve: bool,
        seen: Mutex<Vec<SeenEntry>>,
    }
    #[async_trait]
    impl Approver for ScriptedApprover {
        async fn request(&self, id: Uuid, path: &Path, old: Option<&str>, new: &str) -> bool {
            self.seen.lock().unwrap().push((
                id,
                path.to_path_buf(),
                old.map(|s| s.to_string()),
                new.to_string(),
            ));
            self.approve
        }
    }

    #[tokio::test]
    async fn ask_edit_approved_is_applied_and_emits_request() {
        let reg = registry(); // OneEditCoder → edit to out.txt
        let router = FakeRouter;
        let workspace = RecordingWorkspace::default();
        let tools = ToolRegistry::new(Arc::new(TestAskWriteGate), Arc::new(DenyAsk));
        let approver = ScriptedApprover {
            approve: true,
            seen: Mutex::new(Vec::new()),
        };
        let meter = TokenMeter::default();
        let orch = Orchestrator {
            registry: &reg,
            router: &router,
            workspace: &workspace,
            tools: &tools,
            approver: &approver,
            next_id: &test_id,
            meter: &meter,
            pauser: &NeverPause,
        };

        let events: Arc<Mutex<Vec<EventKind>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = {
            let events = Arc::clone(&events);
            move |kind: EventKind| events.lock().unwrap().push(kind)
        };

        let outcome = orch.run_turn(SessionId::new(), "x", &sink).await.unwrap();
        assert_eq!(outcome, TurnOutcome { ok: true });
        // Approved → edit applied.
        assert_eq!(workspace.edits.lock().unwrap().len(), 1);

        // The approver saw the proposed edit (RecordingWorkspace::read yields empty → old = "").
        let seen = approver.seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].0, Uuid::from_u128(0));
        assert_eq!(seen[0].1, PathBuf::from("out.txt"));
        assert_eq!(seen[0].3, "hi");

        // An ApprovalRequest event was emitted with the same id/path/new.
        let recorded = events.lock().unwrap().clone();
        assert!(recorded.iter().any(|e| matches!(
            e,
            EventKind::ApprovalRequest { id, path, new, .. }
            if *id == Uuid::from_u128(0) && path == &PathBuf::from("out.txt") && new == "hi"
        )));
    }

    #[tokio::test]
    async fn ask_edit_rejected_is_skipped_but_turn_completes() {
        let reg = registry();
        let router = FakeRouter;
        let workspace = RecordingWorkspace::default();
        let tools = ToolRegistry::new(Arc::new(TestAskWriteGate), Arc::new(DenyAsk));
        let approver = ScriptedApprover {
            approve: false,
            seen: Mutex::new(Vec::new()),
        };
        let meter = TokenMeter::default();
        let orch = Orchestrator {
            registry: &reg,
            router: &router,
            workspace: &workspace,
            tools: &tools,
            approver: &approver,
            next_id: &test_id,
            meter: &meter,
            pauser: &NeverPause,
        };

        let events: Arc<Mutex<Vec<EventKind>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = {
            let events = Arc::clone(&events);
            move |kind: EventKind| events.lock().unwrap().push(kind)
        };

        let outcome = orch.run_turn(SessionId::new(), "x", &sink).await.unwrap();
        assert_eq!(outcome, TurnOutcome { ok: true });
        // Rejected → no edit applied.
        assert_eq!(workspace.edits.lock().unwrap().len(), 0);
        let recorded = events.lock().unwrap().clone();
        assert!(recorded.iter().any(|e| matches!(
            e,
            EventKind::Log { message } if message.contains("rejected")
        )));
        assert!(
            !recorded
                .iter()
                .any(|e| matches!(e, EventKind::FileEdit { .. }))
        );
    }

    #[tokio::test]
    async fn emits_cumulative_token_cost_meter_when_usage_present() {
        let reg = registry();
        let router = FakeRouter;
        let workspace = RecordingWorkspace::default();
        let tools = empty_tools();
        let meter = TokenMeter::default();
        // Simulate usage recorded by the MeteringRouter during the turn.
        meter.add(&Usage {
            input_tokens: 3,
            output_tokens: 5,
        });
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
            EventKind::TokenCostMeter {
                input_tokens: 3,
                output_tokens: 5
            }
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
        assert!(
            recorded
                .iter()
                .any(|e| matches!(e, EventKind::Log { message } if message == "turn paused"))
        );
        assert!(
            recorded
                .iter()
                .any(|e| matches!(e, EventKind::Log { message } if message == "turn resumed"))
        );
    }
}
