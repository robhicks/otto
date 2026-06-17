//! The trait seams that keep otto's axes decoupled. `Router` (agent-facing via `AgentCtx`),
//! `Workspace`, and `Agent` are the live seams; `Provider` is the internal LLM-backend trait
//! that routers select among. `RemoteTarget` arrives in a later plan.

use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::router::Router;
use crate::tool::ToolRegistry;
use crate::types::{AgentOutput, AgentRequest, CompleteRequest, CompleteResponse, Edit, WorkspaceSnapshot};

/// An LLM provider (local Ollama, remote Claude, etc.). In-process by default.
#[async_trait]
pub trait Provider: Send + Sync {
    fn id(&self) -> &str;
    async fn complete(&self, req: CompleteRequest) -> anyhow::Result<CompleteResponse>;
}

/// Read access to the repository the engine operates on. This is the agent-facing view
/// (`AgentCtx::workspace()`) — agents may read, but cannot mutate, the workspace.
#[async_trait]
pub trait WorkspaceRead: Send + Sync {
    async fn read(&self, path: &Path) -> anyhow::Result<Vec<u8>>;
    async fn list(&self, glob: &str) -> anyhow::Result<Vec<PathBuf>>;
}

/// The writable repository. `LocalWorkspace` edits a real folder in place (no clone);
/// `RemoteWorkspace` operates on a remote checkout (later plan). Only the orchestrator and the
/// gated `fs.write` tool hold this; agents get the read-only `WorkspaceRead` view.
#[async_trait]
pub trait Workspace: WorkspaceRead {
    /// Apply a full-file edit, returning the number of bytes written.
    async fn apply_edit(&self, edit: &Edit) -> anyhow::Result<u64>;

    /// Capture the workspace's current files as a transferable snapshot, for handover.
    /// Excludes the same paths `list` excludes. (`RemoteWorkspace` reconstitutes from this.)
    async fn snapshot(&self) -> anyhow::Result<WorkspaceSnapshot>;
}

/// A small, single-purpose atomic agent. Native in v1; the trait is the seam where
/// a wasm32-wasip2 agent backend slots in later.
#[async_trait]
pub trait Agent: Send + Sync {
    async fn run(&self, req: AgentRequest, ctx: &AgentCtx) -> anyhow::Result<AgentOutput>;
}

/// Scoped resources an agent may use during a turn. Fields are private; construct via
/// `new` and read via accessors so capabilities can be added without breaking callers.
pub struct AgentCtx<'a> {
    router: &'a dyn Router,
    workspace: &'a dyn WorkspaceRead,
    tools: &'a ToolRegistry,
}

impl<'a> AgentCtx<'a> {
    pub fn new(
        router: &'a dyn Router,
        workspace: &'a dyn WorkspaceRead,
        tools: &'a ToolRegistry,
    ) -> Self {
        Self {
            router,
            workspace,
            tools,
        }
    }

    /// The router agents call to run completions (local-vs-remote selection happens inside).
    pub fn router(&self) -> &dyn Router {
        self.router
    }

    /// The read-only workspace view agents may read from. Writes are NOT available here — they
    /// go through the gated `fs.write` tool or the orchestrator's gated apply.
    pub fn workspace(&self) -> &dyn WorkspaceRead {
        self.workspace
    }

    /// The tool registry; calls made through it are gated by the permission gate before dispatch.
    pub fn tools(&self) -> &ToolRegistry {
        self.tools
    }
}
