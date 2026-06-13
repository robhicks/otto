//! Orchestrator state machine — implementation arrives in Task 7.

/// Outcome of a single orchestrator turn.
#[derive(Debug, Clone, PartialEq)]
pub enum TurnOutcome {
    Continue,
    Done,
}

/// Sink for orchestrator events emitted during a turn.
pub trait Emitter: Send + Sync {
    fn emit(&self, event: &str);
}

/// The orchestrator state machine (stub — filled in Task 7).
pub struct Orchestrator;
