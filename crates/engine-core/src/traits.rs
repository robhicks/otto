//! The trait seams that keep otto's axes decoupled. `Router` (agent-facing via `AgentCtx`),
//! `Workspace`, and `Agent` are the live seams; `Provider` is the internal LLM-backend trait
//! that routers select among. `RemoteTarget` arrives in a later plan.

use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::router::Router;
use crate::types::{AgentOutput, AgentRequest, CompleteRequest, CompleteResponse, Edit};

/// An LLM provider (local Ollama, remote Claude, etc.). In-process by default.
#[async_trait]
pub trait Provider: Send + Sync {
    fn id(&self) -> &str;
    async fn complete(&self, req: CompleteRequest) -> anyhow::Result<CompleteResponse>;
}

/// The repository the engine operates on. `LocalWorkspace` edits a real folder in
/// place (no clone); `RemoteWorkspace` operates on a remote checkout (later plan).
#[async_trait]
pub trait Workspace: Send + Sync {
    async fn read(&self, path: &Path) -> anyhow::Result<Vec<u8>>;
    async fn list(&self, glob: &str) -> anyhow::Result<Vec<PathBuf>>;
    /// Apply a full-file edit, returning the number of bytes written.
    async fn apply_edit(&self, edit: &Edit) -> anyhow::Result<u64>;
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
    workspace: &'a dyn Workspace,
}

impl<'a> AgentCtx<'a> {
    pub fn new(router: &'a dyn Router, workspace: &'a dyn Workspace) -> Self {
        Self { router, workspace }
    }

    /// The router agents call to run completions (local-vs-remote selection happens inside).
    pub fn router(&self) -> &dyn Router {
        self.router
    }

    /// The workspace agents read from / write edits to.
    pub fn workspace(&self) -> &dyn Workspace {
        self.workspace
    }
}
