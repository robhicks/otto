//! otto engine core: the orchestrator state machine and the trait seams it drives.

pub mod meter;
pub mod orchestrator;
pub mod registry;
pub mod retrieval;
pub mod router;
pub mod tool;
pub mod traits;
pub mod types;

pub use meter::TokenMeter;
pub use orchestrator::{Emitter, Orchestrator, TurnOutcome};
pub use registry::AgentRegistry;
pub use retrieval::{Candidate, Retriever};
pub use router::{RouteHints, Router, TaskKind};
pub use tool::{
    AllowListAskResolver, Approver, AskResolver, Decision, DenyApprover, DenyAsk, NeverPause,
    PauseController, PermissionGate, Tool, ToolRegistry,
};
pub use traits::{Agent, AgentCtx, Provider, Workspace, WorkspaceRead};
pub use types::{
    AgentOutput, AgentRequest, CompleteRequest, CompleteResponse, Edit, Milestone, Usage,
};
