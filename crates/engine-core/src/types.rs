//! Plain data passed across the trait seams. No behavior.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// A request to an LLM provider.
#[derive(Debug, Clone, PartialEq)]
pub struct CompleteRequest {
    pub prompt: String,
}

/// A provider's completion.
#[derive(Debug, Clone, PartialEq)]
pub struct CompleteResponse {
    pub text: String,
}

/// A single file edit. For the walking skeleton an edit is a full-file write;
/// real unified-diff application arrives with the real Coder agent in a later plan.
#[derive(Debug, Clone, PartialEq)]
pub struct Edit {
    pub path: PathBuf,
    pub new_contents: String,
}

/// A transferable capture of a workspace's current files (relative path -> contents).
/// Excludes the same paths `list("**")` excludes (`target`/`.git`/`node_modules`/dotfiles).
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
}

/// The uniform structured output returned by any atomic agent.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentOutput {
    Plan { milestones: Vec<Milestone> },
    Context { files: Vec<PathBuf> },
    Code { edits: Vec<Edit> },
    Verify { ok: bool, detail: String },
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
}
