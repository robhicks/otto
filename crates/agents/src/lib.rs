//! otto's atomic agents. `Planner` and `Coder` are real LLM-backed agents — each prompts the
//! router for structured JSON and parses it, falling back safely when no JSON is returned.
//! `Verifier` is real too: it runs `cargo check` via the sandboxed `bash` tool. All four spine
//! agents (`Planner`, `ContextFinder`, `Coder`, `Verifier`) are real.

pub mod coder;
pub mod context_finder;
pub mod parse;
pub mod planner;
pub mod verifier;

pub use coder::Coder;
pub use context_finder::ContextFinder;
pub use planner::Planner;
pub use verifier::Verifier;
