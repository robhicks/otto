//! Session lifecycle over the persistence store. A `Session` owns the durable session
//! identity and the per-session monotonic `seq`/turn counters; running a prompt drives one
//! orchestrator turn and persists its events, turn record, and status. This maps the
//! protocol commands onto store operations: `CreateSession` -> `create`, `SendPrompt` ->
//! `run_prompt`, `Abort` -> `abort`. (Wire-level `Command` dispatch arrives with `serve`.)

use std::sync::{Arc, Mutex};

use otto_engine_core::tool::ToolRegistry;
use otto_engine_core::traits::Workspace;
use otto_engine_core::{AgentRegistry, Orchestrator, Router, TurnOutcome};
use otto_persistence::{SessionStatus, SessionStore, TurnRecord};
use otto_protocol::{Event, EventKind, SessionId};

/// Persistent state for one session. Borrows the store; the engine deps for running a turn
/// are passed to `run_prompt` rather than held, so a `Session` is cheap to create and test.
pub struct Session<'a> {
    store: &'a dyn SessionStore,
    id: SessionId,
    next_seq: u64,
    next_turn: u32,
}

impl<'a> Session<'a> {
    /// Create and persist a new session for `goal` with `config` (provider selection as
    /// JSON). Status starts `Active`. (≙ `Command::CreateSession`.)
    pub async fn create(
        store: &'a dyn SessionStore,
        goal: &str,
        config: &serde_json::Value,
    ) -> anyhow::Result<Session<'a>> {
        let id = store.create_session(goal, config).await?;
        Ok(Session {
            store,
            id,
            next_seq: 0,
            next_turn: 0,
        })
    }

    /// The session's id.
    pub fn id(&self) -> SessionId {
        self.id
    }

    /// Run one orchestrator turn for `goal`, persisting the turn's events (fail-closed: a
    /// store error fails the turn), then recording the turn and updating status to `Done`
    /// (or `Failed`). The per-session `seq` counter continues across calls. Returns the
    /// turn's events and outcome. (≙ `Command::SendPrompt`.)
    pub async fn run_prompt(
        &mut self,
        registry: &AgentRegistry,
        router: &dyn Router,
        workspace: &dyn Workspace,
        tools: &ToolRegistry,
        goal: &str,
    ) -> anyhow::Result<(Vec<Event>, TurnOutcome)> {
        let store = self.store;
        let id = self.id;

        let collected: Arc<Mutex<Vec<Event>>> = Arc::new(Mutex::new(Vec::new()));
        let seq = Arc::new(Mutex::new(self.next_seq));
        let sink = {
            let collected = Arc::clone(&collected);
            let seq = Arc::clone(&seq);
            move |kind: EventKind| {
                let mut s = seq.lock().unwrap();
                collected.lock().unwrap().push(Event {
                    seq: *s,
                    session: id,
                    kind,
                });
                *s += 1;
            }
        };

        let orchestrator = Orchestrator {
            registry,
            router,
            workspace,
            tools,
        };
        let outcome = match orchestrator.run_turn(id, goal, &sink).await {
            Ok(outcome) => outcome,
            Err(e) => {
                // Orchestrator failure: mark the session Failed (best-effort) before
                // propagating, so a crashed turn doesn't leave the session stuck Active.
                let _ = store.set_status(id, SessionStatus::Failed).await;
                return Err(e);
            }
        };
        let events = collected.lock().unwrap().clone();

        // Persist this turn's events. Fail-closed: a store error fails the turn rather than
        // silently dropping events (the durable log is the whole point of the store).
        for event in &events {
            store.append_event(id, event).await?;
        }
        self.next_seq = *seq.lock().unwrap();

        store
            .record_turn(
                id,
                &TurnRecord {
                    turn_index: self.next_turn,
                    goal: goal.to_string(),
                    outcome: serde_json::json!({ "ok": outcome.ok }),
                },
            )
            .await?;
        self.next_turn += 1;

        let status = if outcome.ok {
            SessionStatus::Done
        } else {
            SessionStatus::Failed
        };
        store.set_status(id, status).await?;

        Ok((events, outcome))
    }

    /// Mark the session aborted. (≙ `Command::Abort`.)
    pub async fn abort(&self) -> anyhow::Result<()> {
        self.store.set_status(self.id, SessionStatus::Aborted).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use otto_persistence::SqliteStore;
    use otto_providers::ScriptedProvider;
    use otto_router::SingleProviderRouter;
    use otto_workspace::LocalWorkspace;
    use std::sync::Arc;

    async fn store_in(dir: &tempfile::TempDir) -> SqliteStore {
        SqliteStore::open(dir.path().join("sessions.db"))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn create_persists_active_session() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(&dir).await;
        let session = Session::create(&store, "do a thing", &serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(
            store.session_status(session.id()).await.unwrap(),
            SessionStatus::Active
        );
    }

    #[tokio::test]
    async fn run_prompt_persists_events_and_marks_done() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(&dir).await;

        // Scripted model: planner prompt contains "milestones", coder prompt contains "edits".
        let provider = ScriptedProvider::new("{}")
            .on(
                "edits",
                r#"{"edits": [{"path": "out.txt", "contents": "hi add a greeting"}]}"#,
            )
            .on(
                "milestones",
                r#"{"milestones": [{"description": "write it"}]}"#,
            );
        let router = SingleProviderRouter::new(Arc::new(provider));
        let workspace = LocalWorkspace::new(dir.path());
        let tools_ws: Arc<dyn otto_engine_core::traits::Workspace> =
            Arc::new(LocalWorkspace::new(dir.path()));
        let tools = crate::build_tool_registry(tools_ws, dir.path().to_path_buf());
        let registry = crate::build_default_registry();

        let mut session = Session::create(&store, "add a greeting", &serde_json::json!({}))
            .await
            .unwrap();
        let id = session.id();
        let (events, outcome) = session
            .run_prompt(&registry, &router, &workspace, &tools, "add a greeting")
            .await
            .unwrap();

        assert!(outcome.ok);
        assert_eq!(store.session_status(id).await.unwrap(), SessionStatus::Done);

        // The persisted log equals the returned events, with contiguous seqs from 0.
        let replayed = store.replay_since(id, None).await.unwrap();
        assert_eq!(replayed, events);
        assert!(!events.is_empty());
        for (i, event) in events.iter().enumerate() {
            assert_eq!(event.seq, i as u64);
        }
    }

    #[tokio::test]
    async fn second_prompt_continues_seq() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(&dir).await;
        let provider = ScriptedProvider::new("{}")
            .on(
                "edits",
                r#"{"edits": [{"path": "out.txt", "contents": "hi g"}]}"#,
            )
            .on("milestones", r#"{"milestones": [{"description": "x"}]}"#);
        let router = SingleProviderRouter::new(Arc::new(provider));
        let workspace = LocalWorkspace::new(dir.path());
        let tools_ws: Arc<dyn otto_engine_core::traits::Workspace> =
            Arc::new(LocalWorkspace::new(dir.path()));
        let tools = crate::build_tool_registry(tools_ws, dir.path().to_path_buf());
        let registry = crate::build_default_registry();

        let mut session = Session::create(&store, "g", &serde_json::json!({}))
            .await
            .unwrap();
        let id = session.id();
        let (turn1, _) = session
            .run_prompt(&registry, &router, &workspace, &tools, "g")
            .await
            .unwrap();
        let (turn2, _) = session
            .run_prompt(&registry, &router, &workspace, &tools, "g")
            .await
            .unwrap();

        let last1 = turn1.last().unwrap().seq;
        // Turn 2's first event continues right after turn 1's last.
        assert_eq!(turn2.first().unwrap().seq, last1 + 1);

        // The full replayed log is contiguous from 0 and covers both turns.
        let all = store.replay_since(id, None).await.unwrap();
        assert_eq!(all.len(), turn1.len() + turn2.len());
        for (i, event) in all.iter().enumerate() {
            assert_eq!(event.seq, i as u64);
        }

        // Replaying after turn 1's last seq yields exactly turn 2.
        let gap = store.replay_since(id, Some(last1)).await.unwrap();
        assert_eq!(gap, turn2);
    }

    #[tokio::test]
    async fn abort_sets_status_aborted() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(&dir).await;
        let session = Session::create(&store, "g", &serde_json::json!({}))
            .await
            .unwrap();
        session.abort().await.unwrap();
        assert_eq!(
            store.session_status(session.id()).await.unwrap(),
            SessionStatus::Aborted
        );
    }

    #[tokio::test]
    async fn orchestrator_error_marks_session_failed() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(&dir).await;
        // An empty registry: the orchestrator can't find the Planner, so run_turn errors.
        let registry = otto_engine_core::AgentRegistry::new();
        let provider = ScriptedProvider::new("{}");
        let router = SingleProviderRouter::new(Arc::new(provider));
        let workspace = LocalWorkspace::new(dir.path());
        let tools_ws: Arc<dyn otto_engine_core::traits::Workspace> =
            Arc::new(LocalWorkspace::new(dir.path()));
        let tools = crate::build_tool_registry(tools_ws, dir.path().to_path_buf());

        let mut session = Session::create(&store, "g", &serde_json::json!({}))
            .await
            .unwrap();
        let id = session.id();
        let result = session
            .run_prompt(&registry, &router, &workspace, &tools, "g")
            .await;
        assert!(result.is_err());
        assert_eq!(
            store.session_status(id).await.unwrap(),
            SessionStatus::Failed
        );
    }
}
