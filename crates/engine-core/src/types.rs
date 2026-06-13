//! Plain data passed across the trait seams. No behavior.

use std::path::PathBuf;

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

/// One unit of a plan.
#[derive(Debug, Clone, PartialEq)]
pub struct Milestone {
    pub description: String,
}

/// The uniform request passed to any atomic agent.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentRequest {
    Plan { goal: String },
    FindContext { goal: String },
    Code { goal: String, context: Vec<PathBuf> },
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
