//! Loads otto's native extension format — Claude Code's `.claude/` convention — and
//! registers each artifact into an existing otto primitive. Slice 1: custom agents.
//!
//! This crate is a leaf: it depends inward on `engine-core`/`protocol` and is wired only
//! by the `engine` binary, never by `engine-core`. The orchestrator core never calls
//! discovery, so the offline determinism suite is unaffected.

mod agent_def;

pub use agent_def::{CustomAgentDef, parse_agent_md};
