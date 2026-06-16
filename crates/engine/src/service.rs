//! `EngineService`: the transport-agnostic core that runs prompts with live event streaming
//! and persistence. Owns the session store and the shared engine deps; both the CLI and the
//! serve layer drive it. Maps the protocol commands onto operations: `CreateSession` ->
//! `create_session`, `SendPrompt` -> `run_prompt`, `Abort` -> `abort`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use otto_engine_core::tool::ToolRegistry;
use otto_engine_core::traits::Workspace;
use otto_engine_core::{AgentRegistry, Orchestrator, Router, TurnOutcome};
use otto_persistence::{SessionStatus, SessionStore, TurnRecord};
use otto_protocol::{Event, EventKind, SessionId};

/// Receives a turn's events in seq order, each AFTER it is durably persisted. The CLI uses a
/// collecting sink; the serve layer uses one that writes to a WebSocket.
#[async_trait]
pub trait EventSink: Send {
    async fn emit(&mut self, event: &Event) -> anyhow::Result<()>;
}

/// An `EventSink` that gathers events into a `Vec` (used by the CLI / tests).
#[derive(Default)]
pub struct CollectingSink {
    pub events: Vec<Event>,
}

#[async_trait]
impl EventSink for CollectingSink {
    async fn emit(&mut self, event: &Event) -> anyhow::Result<()> {
        self.events.push(event.clone());
        Ok(())
    }
}

/// Runs sessions against a store and a fixed set of engine deps. One turn at a time
/// (`turn_lock`), because the workspace is shared mutable state.
pub struct EngineService {
    store: Arc<dyn SessionStore>,
    registry: Arc<AgentRegistry>,
    router: Arc<dyn Router>,
    workspace: Arc<dyn Workspace>,
    tools: Arc<ToolRegistry>,
    turn_lock: tokio::sync::Mutex<()>,
}

impl EngineService {
    pub fn new(
        store: Arc<dyn SessionStore>,
        registry: Arc<AgentRegistry>,
        router: Arc<dyn Router>,
        workspace: Arc<dyn Workspace>,
        tools: Arc<ToolRegistry>,
    ) -> Self {
        Self {
            store,
            registry,
            router,
            workspace,
            tools,
            turn_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// The session store, for reads the serve layer needs (e.g. replay on reconnect).
    pub fn store(&self) -> &dyn SessionStore {
        &*self.store
    }

    /// Create and persist a new session. (≙ `Command::CreateSession`.)
    pub async fn create_session(
        &self,
        goal: &str,
        config: &serde_json::Value,
    ) -> anyhow::Result<SessionId> {
        self.store.create_session(goal, config).await
    }

    /// Mark a session aborted. (≙ `Command::Abort`.)
    pub async fn abort(&self, session: SessionId) -> anyhow::Result<()> {
        self.store.set_status(session, SessionStatus::Aborted).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use otto_persistence::SqliteStore;
    use otto_providers::ScriptedProvider;
    use otto_router::SingleProviderRouter;
    use otto_workspace::LocalWorkspace;

    fn scripted_router() -> Arc<dyn Router> {
        let provider = ScriptedProvider::new("{}")
            .on(
                "edits",
                r#"{"edits": [{"path": "out.txt", "contents": "hi g"}]}"#,
            )
            .on("milestones", r#"{"milestones": [{"description": "x"}]}"#);
        Arc::new(SingleProviderRouter::new(Arc::new(provider)))
    }

    async fn service_in(dir: &tempfile::TempDir, registry: AgentRegistry) -> EngineService {
        let store: Arc<dyn SessionStore> =
            Arc::new(SqliteStore::open(dir.path().join("s.db")).await.unwrap());
        let workspace: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(dir.path()));
        let tools_ws: Arc<dyn Workspace> = Arc::new(LocalWorkspace::new(dir.path()));
        let tools = Arc::new(crate::build_tool_registry(
            tools_ws,
            dir.path().to_path_buf(),
        ));
        EngineService::new(store, Arc::new(registry), scripted_router(), workspace, tools)
    }

    #[tokio::test]
    async fn create_persists_active_session() {
        let dir = tempfile::tempdir().unwrap();
        let service = service_in(&dir, crate::build_default_registry()).await;
        let id = service
            .create_session("do a thing", &serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(
            service.store().session_status(id).await.unwrap(),
            SessionStatus::Active
        );
    }

    #[tokio::test]
    async fn abort_sets_status_aborted() {
        let dir = tempfile::tempdir().unwrap();
        let service = service_in(&dir, crate::build_default_registry()).await;
        let id = service
            .create_session("g", &serde_json::json!({}))
            .await
            .unwrap();
        service.abort(id).await.unwrap();
        assert_eq!(
            service.store().session_status(id).await.unwrap(),
            SessionStatus::Aborted
        );
    }
}
