//! The routing seam. Agents call a `Router` (not a single `Provider`) so the engine
//! can pick local vs remote per request — otto's "Brain-Blend". `engine-core` owns the
//! trait; concrete routers live in the `otto-router` crate.

use async_trait::async_trait;

use crate::types::{CompleteRequest, CompleteResponse};

/// The kind of work a request represents. Influences local-vs-remote routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TaskKind {
    /// Mechanical, low-stakes generation. Cheapest tier; prefers local.
    #[default]
    Boilerplate,
    /// A focused code edit. Mid tier.
    Edit,
    /// Cross-cutting or design-level reasoning. Prefers a frontier remote model.
    Architecture,
}

/// Inputs the orchestrator/agents supply to influence routing. All optional-ish with
/// sensible defaults so callers only set what they know.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RouteHints {
    pub task_kind: TaskKind,
    /// Rough size of the request context, in tokens. 0 if unknown.
    pub token_estimate: usize,
    /// If true, the request touches sensitive data and MUST stay local.
    pub privacy_sensitive: bool,
    /// How many times this logical step has already failed. Drives escalation.
    pub prior_failures: u32,
}

/// Selects a provider per request and runs the completion. The agent-facing seam.
#[async_trait]
pub trait Router: Send + Sync {
    async fn complete(
        &self,
        req: CompleteRequest,
        hints: RouteHints,
    ) -> anyhow::Result<CompleteResponse>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_hints_default_is_boilerplate_and_zeroed() {
        let h = RouteHints::default();
        assert_eq!(h.task_kind, TaskKind::Boilerplate);
        assert_eq!(h.token_estimate, 0);
        assert!(!h.privacy_sensitive);
        assert_eq!(h.prior_failures, 0);
    }
}
