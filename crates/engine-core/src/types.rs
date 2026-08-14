//! Plain data passed across the trait seams. No behavior.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// A request to an LLM provider.
#[derive(Debug, Clone, PartialEq)]
pub struct CompleteRequest {
    pub prompt: String,
}

/// Token usage reported by a provider for one completion. Absent for providers that do not
/// report it (the offline `LocalProvider`/`ScriptedProvider`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: u32,
    pub output_tokens: u32,
}

/// A provider's completion.
#[derive(Debug, Clone, PartialEq)]
pub struct CompleteResponse {
    pub text: String,
    /// Token usage, when the provider reports it. `None` on the offline/deterministic path.
    pub usage: Option<Usage>,
}

/// A single file edit. For the walking skeleton an edit is a full-file write;
/// real unified-diff application arrives with the real Coder agent in a later plan.
#[derive(Debug, Clone, PartialEq)]
pub struct Edit {
    pub path: PathBuf,
    pub new_contents: String,
}

/// A transferable capture of a workspace's current files (relative path -> contents).
///
/// **Scope of the exclusion guarantee.** A snapshot *produced by a `Workspace` impl* excludes the
/// same paths `list("**")` excludes (`target`/`.git`/`node_modules`/dotfiles) **and** every path
/// the sensitive-path floor marks — the dotfile skip does not imply the floor (see
/// `Workspace::snapshot`). A snapshot *deserialized from a peer* carries no such guarantee: this
/// type is `Deserialize` with a public field, and `otto_remote::PromoteBundle` is parsed straight
/// off the wire by `POST /promote`. Ingress is validated separately by
/// `EngineService::validate_workspace_edits`, which is why that check must not be removed on the
/// theory that snapshots are already clean.
///
/// Serde-serializable so it can later cross the wire to a remote engine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceSnapshot {
    // TODO(remote): Vec<u8> serializes as a JSON int array; switch to base64 when
    // WorkspaceSnapshot starts crossing the wire (RemoteWorkspace).
    pub files: Vec<(PathBuf, Vec<u8>)>,
}

/// One unit of a plan.
#[derive(Debug, Clone, PartialEq)]
pub struct Milestone {
    pub description: String,
}

/// The verifier's result for a turn, retained on `TurnOutcome` so conversation history can
/// report it without re-scanning the event log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VerifySummary {
    pub ok: bool,
    pub detail: String,
}

/// The uniform request passed to any atomic agent.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentRequest {
    Plan {
        goal: String,
    },
    FindContext {
        goal: String,
    },
    Code {
        goal: String,
        /// The planned milestones (from the Planner), giving the Coder the intended steps.
        milestones: Vec<Milestone>,
        context: Vec<PathBuf>,
        /// The previous verify failure detail, if this is a repair attempt.
        feedback: Option<String>,
        /// How many times this turn has already failed verification (drives routing escalation).
        prior_failures: u32,
    },
    Verify,
    /// A free-form subagent task (custom agents). The fixed spine never constructs this.
    Task {
        prompt: String,
    },
}

/// The uniform structured output returned by any atomic agent.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentOutput {
    Plan {
        milestones: Vec<Milestone>,
    },
    Context {
        files: Vec<PathBuf>,
    },
    Code {
        edits: Vec<Edit>,
    },
    Verify {
        ok: bool,
        detail: String,
    },
    /// A free-form subagent result (custom agents).
    Task {
        text: String,
    },
}

/// How many prior turns conversation history carries. Bounded so prompt size does not grow
/// with session length — a 200-turn session must not produce a 200-turn prompt.
pub const HISTORY_TURNS: usize = 10;

/// How many edited paths a single remembered turn contributes.
pub const HISTORY_FILES_PER_TURN: usize = 20;

/// One prior turn, as the spine remembers it.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnSummary {
    pub turn_index: u32,
    pub goal: String,
    pub milestones: Vec<String>,
    pub files_edited: Vec<PathBuf>,
    pub verify: Option<VerifySummary>,
    pub ok: bool,
}

/// The bounded conversation history handed to agents through `AgentCtx`. Construct via `new`
/// (which applies the bounds) or `empty`; the field is private so the bounds cannot be bypassed.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SessionHistory {
    turns: Vec<TurnSummary>,
}

impl SessionHistory {
    /// A shared empty history, so `AgentCtx::history()` can return a reference without allocating.
    pub const EMPTY: SessionHistory = SessionHistory { turns: Vec::new() };

    /// Retain the most recent `HISTORY_TURNS` turns, each truncated to
    /// `HISTORY_FILES_PER_TURN` paths.
    pub fn new(mut turns: Vec<TurnSummary>) -> Self {
        if turns.len() > HISTORY_TURNS {
            turns.drain(..turns.len() - HISTORY_TURNS);
        }
        for t in &mut turns {
            t.files_edited.truncate(HISTORY_FILES_PER_TURN);
        }
        Self { turns }
    }

    pub fn empty() -> Self {
        Self { turns: Vec::new() }
    }

    pub fn is_empty(&self) -> bool {
        self.turns.is_empty()
    }

    pub fn turns(&self) -> &[TurnSummary] {
        &self.turns
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_snapshot_round_trips_through_json() {
        let snap = WorkspaceSnapshot {
            files: vec![
                (PathBuf::from("a.txt"), b"hello".to_vec()),
                (PathBuf::from("src/lib.rs"), vec![0, 1, 2, 255]),
            ],
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: WorkspaceSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snap, back);
    }

    #[test]
    fn session_history_keeps_only_the_last_n_turns() {
        let turns: Vec<TurnSummary> = (0..25)
            .map(|i| TurnSummary {
                turn_index: i,
                goal: format!("goal {i}"),
                milestones: vec![],
                files_edited: vec![],
                verify: None,
                ok: true,
            })
            .collect();

        let h = SessionHistory::new(turns);
        assert_eq!(h.turns().len(), HISTORY_TURNS);
        // The most recent turns are the ones kept.
        assert_eq!(h.turns().first().unwrap().turn_index, 15);
        assert_eq!(h.turns().last().unwrap().turn_index, 24);
    }

    #[test]
    fn session_history_caps_files_per_turn() {
        let turns = vec![TurnSummary {
            turn_index: 0,
            goal: "g".to_string(),
            milestones: vec![],
            files_edited: (0..100)
                .map(|i| PathBuf::from(format!("f{i}.rs")))
                .collect(),
            verify: None,
            ok: true,
        }];
        let h = SessionHistory::new(turns);
        assert_eq!(h.turns()[0].files_edited.len(), HISTORY_FILES_PER_TURN);
    }

    #[test]
    fn empty_history_is_empty() {
        assert!(SessionHistory::empty().is_empty());
    }
}
