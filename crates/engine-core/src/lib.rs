//! otto engine core: the orchestrator state machine and the trait seams it drives.

pub mod orchestrator;
pub mod registry;
pub mod router;
pub mod traits;
pub mod types;

pub use orchestrator::{Emitter, Orchestrator, TurnOutcome};
pub use registry::AgentRegistry;
pub use router::{RouteHints, Router, TaskKind};
pub use traits::{Agent, AgentCtx, Provider, Workspace};
pub use types::{AgentOutput, AgentRequest, CompleteRequest, CompleteResponse, Edit, Milestone};
