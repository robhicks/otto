//! otto engine core: the orchestrator state machine and the trait seams it drives.

pub mod auth;
pub mod meter;
pub mod orchestrator;
pub mod registry;
pub mod retrieval;
pub mod router;
pub mod tool;
pub mod traits;
pub mod types;

pub use auth::{AuthConfig, AuthError, Authenticator, Principal, TokenPair};
pub use meter::TokenMeter;
pub use orchestrator::{Emitter, Orchestrator, TurnOutcome};
pub use registry::AgentRegistry;
pub use retrieval::{Candidate, Retriever};
pub use router::{RouteHints, Router, TaskKind};
// The sensitive-path floor lives in `protocol` (the dependency-free leaf crate) so tools that
// can't take an engine-core dependency can still share it; re-exported here for existing callers.
pub use otto_protocol::{SENSITIVE_MARKERS, is_sensitive};
pub use tool::{
    AllowListAskResolver, Approver, AskResolver, Decision, DenyApprover, DenyAsk, NeverPause,
    PauseController, PermissionGate, Tool, ToolRegistry,
};
pub use traits::{Agent, AgentCtx, Provider, Workspace, WorkspaceRead};
pub use types::{
    AgentOutput, AgentRequest, CompleteRequest, CompleteResponse, Edit, Milestone, Usage,
};
