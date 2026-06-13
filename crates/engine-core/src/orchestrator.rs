//! The deterministic orchestrator spine: Plan -> Execute -> Verify -> Done.
//! It owns control flow and event emission; capabilities live in the agents.

use otto_protocol::{EventKind, Role, SessionId};

use crate::registry::AgentRegistry;
use crate::router::Router;
use crate::tool::ToolRegistry;
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
}

impl<'a> Orchestrator<'a> {
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

        // --- Execute ---
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

        emit.emit(EventKind::AgentStarted { role: Role::Coder });
        let coded = self
            .registry
            .get(&Role::Coder)?
            .run(
                AgentRequest::Code {
                    goal: goal.to_string(),
                    context: files,
                },
                &ctx,
            )
            .await?;
        let AgentOutput::Code { edits } = coded else {
            anyhow::bail!("coder returned unexpected output");
        };
        for edit in &edits {
            let bytes_written = self.workspace.apply_edit(edit).await?;
            emit.emit(EventKind::FileEdit {
                path: edit.path.clone(),
                bytes_written,
            });
        }
        emit.emit(EventKind::AgentFinished { role: Role::Coder });

        // --- Verify ---
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
        emit.emit(EventKind::VerifyResult { ok, detail });
        emit.emit(EventKind::AgentFinished {
            role: Role::Verifier,
        });

        // --- Done ---
        emit.emit(EventKind::TurnComplete { ok });
        Ok(TurnOutcome { ok })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::router::{RouteHints, Router};
    use crate::tool::{Decision, DenyAsk, PermissionGate, ToolRegistry};
    use crate::traits::{Agent, Workspace};
    use crate::types::{CompleteRequest, CompleteResponse, Edit, Milestone};
    use async_trait::async_trait;
    use serde_json::Value;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    struct TestAllowGate;
    impl PermissionGate for TestAllowGate {
        fn evaluate(&self, _tool: &str, _args: &Value) -> Decision {
            Decision::Allow
        }
    }
    fn empty_tools() -> ToolRegistry {
        ToolRegistry::new(Arc::new(TestAllowGate), Arc::new(DenyAsk))
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
            })
        }
    }

    #[derive(Default)]
    struct RecordingWorkspace {
        edits: Mutex<Vec<Edit>>,
    }
    #[async_trait]
    impl Workspace for RecordingWorkspace {
        async fn read(&self, _path: &Path) -> anyhow::Result<Vec<u8>> {
            Ok(Vec::new())
        }
        async fn list(&self, _glob: &str) -> anyhow::Result<Vec<PathBuf>> {
            Ok(Vec::new())
        }
        async fn apply_edit(&self, edit: &Edit) -> anyhow::Result<u64> {
            self.edits.lock().unwrap().push(edit.clone());
            Ok(edit.new_contents.len() as u64)
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
        let orch = Orchestrator {
            registry: &reg,
            router: &router,
            workspace: &workspace,
            tools: &tools,
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
        let orch = Orchestrator {
            registry: &reg,
            router: &router,
            workspace: &workspace,
            tools: &tools,
        };

        let err = orch
            .run_turn(SessionId::new(), "x", &(|_k: EventKind| {}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no agent registered"));
    }
}
