//! Engine wiring: assemble the default agent registry and run a turn end-to-end.

use std::sync::{Arc, Mutex};

use otto_agents::{EchoCoder, StubContextFinder, StubPlanner, StubVerifier};
use otto_engine_core::traits::{Provider, Workspace};
use otto_engine_core::{AgentRegistry, Orchestrator, TurnOutcome};
use otto_protocol::{Event, EventKind, Role, SessionId};

/// Build the registry of built-in walking-skeleton agents.
pub fn build_default_registry() -> AgentRegistry {
    let mut registry = AgentRegistry::new();
    registry.register(Role::Planner, Arc::new(StubPlanner));
    registry.register(Role::ContextFinder, Arc::new(StubContextFinder));
    registry.register(Role::Coder, Arc::new(EchoCoder));
    registry.register(Role::Verifier, Arc::new(StubVerifier));
    registry
}

/// Run one turn for `goal` against `workspace` using `provider`, returning the
/// sequenced events emitted and the final outcome. The engine assigns the per-session
/// monotonic `seq` to each event here (the orchestrator emits bare `EventKind`s).
pub async fn run_goal(
    goal: &str,
    provider: &dyn Provider,
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
            collected.lock().unwrap().push(Event {
                seq: *seq,
                session,
                kind,
            });
            *seq += 1;
        }
    };

    let orchestrator = Orchestrator {
        registry: &registry,
        provider,
        workspace,
    };
    let outcome = orchestrator.run_turn(session, goal, &sink).await?;

    let events = collected.lock().unwrap().clone();
    Ok((events, outcome))
}
