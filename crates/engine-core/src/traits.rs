//! The trait seams that keep otto's axes decoupled. `Router` (agent-facing via `AgentCtx`),
//! `Workspace`, and `Agent` are the live seams; `Provider` is the internal LLM-backend trait
//! that routers select among. `RemoteTarget` arrives in a later plan.

use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::router::Router;
use crate::tool::ToolRegistry;
use crate::types::{
    AgentOutput, AgentRequest, CompleteRequest, CompleteResponse, Edit, WorkspaceSnapshot,
};

/// An LLM provider (local Ollama, remote Claude, etc.). In-process by default.
#[async_trait]
pub trait Provider: Send + Sync {
    fn id(&self) -> &str;
    async fn complete(&self, req: CompleteRequest) -> anyhow::Result<CompleteResponse>;
}

/// Read access to the repository the engine operates on. This is the agent-facing view
/// (`AgentCtx::workspace()`) — agents may read, but cannot mutate, the workspace.
///
/// `read` and `list` are deliberately **not** floor-filtered; only `Workspace::snapshot` is,
/// because it alone ships whole file contents off-machine. Access through these two is mediated
/// by the gated `fs.read`/`fs.list` tools, which do enforce the floor. Do not read
/// `snapshot`'s guarantee as covering the seam.
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
    /// Excludes the same paths `list` excludes, **and** every path the sensitive-path floor
    /// marks (`otto_protocol::is_sensitive`) — the two are not the same, since the floor's
    /// markers match as substrings, so `id_rsa`/`production.env` survive `list`'s dotfile skip.
    /// An impl must apply the floor: a snapshot is what leaves the machine in a promote bundle.
    /// (`RemoteWorkspace` reconstitutes from this.)
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
    retriever: Option<&'a dyn crate::retrieval::Retriever>,
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
            retriever: None,
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

    /// Attach a retriever (the indexed candidate source). Absent → agents use their fallback.
    pub fn with_retriever(mut self, retriever: &'a dyn crate::retrieval::Retriever) -> Self {
        self.retriever = Some(retriever);
        self
    }

    /// The retriever, if one is wired. `None` keeps the deterministic offline fallback path.
    pub fn retriever(&self) -> Option<&dyn crate::retrieval::Retriever> {
        self.retriever
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use async_trait::async_trait;

    use super::*;
    use crate::retrieval::Candidate;
    use crate::router::{RouteHints, Router};
    use crate::tool::{Decision, DenyAsk, PermissionGate, ToolRegistry};
    use crate::types::{CompleteRequest, CompleteResponse};

    struct StubRouter;
    #[async_trait]
    impl Router for StubRouter {
        async fn complete(
            &self,
            _req: CompleteRequest,
            _hints: RouteHints,
        ) -> anyhow::Result<CompleteResponse> {
            Ok(CompleteResponse {
                text: String::new(),
                usage: None,
            })
        }
    }

    struct StubWorkspace;
    #[async_trait]
    impl WorkspaceRead for StubWorkspace {
        async fn read(&self, _path: &Path) -> anyhow::Result<Vec<u8>> {
            Ok(Vec::new())
        }
        async fn list(&self, _glob: &str) -> anyhow::Result<Vec<PathBuf>> {
            Ok(Vec::new())
        }
    }

    struct AllowGate;
    impl PermissionGate for AllowGate {
        fn evaluate(&self, _tool: &str, _args: &serde_json::Value) -> Decision {
            Decision::Allow
        }
    }

    fn stub_tools() -> ToolRegistry {
        ToolRegistry::new(Arc::new(AllowGate), Arc::new(DenyAsk))
    }

    struct NoopRetriever;
    #[async_trait]
    impl crate::retrieval::Retriever for NoopRetriever {
        async fn search(&self, _goal: &str, _limit: usize) -> anyhow::Result<Vec<Candidate>> {
            Ok(vec![])
        }
    }

    #[test]
    fn retriever_defaults_to_none() {
        let router = StubRouter;
        let ws = StubWorkspace;
        let tools = stub_tools();
        let ctx = AgentCtx::new(&router, &ws, &tools);
        assert!(ctx.retriever().is_none());
    }

    #[test]
    fn with_retriever_sets_some() {
        let router = StubRouter;
        let ws = StubWorkspace;
        let tools = stub_tools();
        let noop = NoopRetriever;
        let ctx = AgentCtx::new(&router, &ws, &tools).with_retriever(&noop);
        assert!(ctx.retriever().is_some());
    }
}
