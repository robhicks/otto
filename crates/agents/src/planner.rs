//! The Planner agent: prompts the router to decompose a goal into milestones, parsing a
//! structured JSON response. Falls back to the whole goal as one milestone on parse failure.

use async_trait::async_trait;
use otto_engine_core::router::{RouteHints, TaskKind};
use otto_engine_core::traits::{Agent, AgentCtx};
use otto_engine_core::types::{
    AgentOutput, AgentRequest, CompleteRequest, Milestone, SessionHistory,
};
use serde::Deserialize;

use crate::parse::{extract_json, history_block};

pub struct Planner;

#[derive(Deserialize)]
struct PlanResponse {
    milestones: Vec<MilestoneDto>,
}

#[derive(Deserialize)]
struct MilestoneDto {
    description: String,
}

fn plan_prompt(goal: &str, history: &SessionHistory) -> String {
    format!(
        "You are otto's planner. Decompose the goal into an ordered list of concrete milestones.\n\
         Goal: {goal}{history}\n\
         Respond ONLY with valid JSON matching this schema:\n\
         milestones: array of objects, each with a string field named description.",
        history = history_block(history)
    )
}

#[async_trait]
impl Agent for Planner {
    async fn run(&self, req: AgentRequest, ctx: &AgentCtx) -> anyhow::Result<AgentOutput> {
        let AgentRequest::Plan { goal } = req else {
            anyhow::bail!("Planner received a non-Plan request");
        };
        let completion = ctx
            .router()
            .complete(
                CompleteRequest {
                    prompt: plan_prompt(&goal, ctx.history()),
                },
                RouteHints {
                    task_kind: TaskKind::Architecture,
                    ..RouteHints::default()
                },
            )
            .await?;
        // Parse the structured plan; on any failure or empty plan, fall back to the whole
        // goal as a single milestone so the turn can still proceed.
        let milestones = match extract_json::<PlanResponse>(&completion.text) {
            Ok(plan) if !plan.milestones.is_empty() => plan
                .milestones
                .into_iter()
                .map(|m| Milestone {
                    description: m.description,
                })
                .collect(),
            _ => vec![Milestone { description: goal }],
        };
        Ok(AgentOutput::Plan { milestones })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use otto_engine_core::tool::{DenyAsk, ToolRegistry};
    use otto_engine_core::types::{SessionHistory, TurnSummary, VerifySummary};
    use otto_providers::{LocalProvider, ScriptedProvider};
    use otto_router::SingleProviderRouter;
    use otto_tools::DefaultPermissionGate;
    use otto_workspace::LocalWorkspace;
    use std::path::PathBuf;
    use std::sync::Arc;

    async fn run_planner(router: &SingleProviderRouter, goal: &str) -> Vec<Milestone> {
        let dir = tempfile::tempdir().unwrap();
        let ws = LocalWorkspace::new(dir.path());
        let tools = ToolRegistry::new(Arc::new(DefaultPermissionGate::new()), Arc::new(DenyAsk));
        let ctx = AgentCtx::new(router, &ws, &tools);
        let out = Planner
            .run(
                AgentRequest::Plan {
                    goal: goal.to_string(),
                },
                &ctx,
            )
            .await
            .unwrap();
        match out {
            AgentOutput::Plan { milestones } => milestones,
            other => panic!("expected Plan, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn parses_milestones_from_json() {
        let provider = ScriptedProvider::new("{}").on(
            "milestones",
            r#"{"milestones": [{"description": "step one"}, {"description": "step two"}]}"#,
        );
        let router = SingleProviderRouter::new(Arc::new(provider));
        let milestones = run_planner(&router, "build a thing").await;
        assert_eq!(milestones.len(), 2);
        assert_eq!(milestones[0].description, "step one");
        assert_eq!(milestones[1].description, "step two");
    }

    /// The single source of truth for the pinned no-history prompt string. Both the pinning
    /// test and the empty-history regression test assert against this literal.
    fn plan_prompt_pinned_expectation() -> String {
        "You are otto's planner. Decompose the goal into an ordered list of concrete milestones.\n\
         Goal: add a hello function\n\
         Respond ONLY with valid JSON matching this schema:\n\
         milestones: array of objects, each with a string field named description."
            .to_string()
    }

    #[test]
    fn plan_prompt_is_pinned_for_the_no_history_case() {
        // Regression rail for threading conversation history into the Planner: the offline
        // suite asserts on LocalProvider output, which echoes this exact prompt. Do NOT
        // "helpfully" update this string when the history change makes it fail — that means
        // the no-history path changed, which the history task is required to leave alone.
        assert_eq!(
            plan_prompt("add a hello function", &SessionHistory::empty()),
            plan_prompt_pinned_expectation()
        );
    }

    #[test]
    fn plan_prompt_with_empty_history_is_unchanged() {
        // The rail: the no-history prompt must equal the pinned string from Task 5.
        assert_eq!(
            plan_prompt("add a hello function", &SessionHistory::empty()),
            plan_prompt_pinned_expectation()
        );
    }

    #[test]
    fn plan_prompt_with_history_names_prior_goals_and_files() {
        let history = SessionHistory::new(vec![TurnSummary {
            turn_index: 0,
            goal: "add a hello function".to_string(),
            milestones: vec!["create hello.rs".to_string()],
            files_edited: vec![PathBuf::from("hello.rs")],
            verify: Some(VerifySummary {
                ok: true,
                detail: "passed".to_string(),
            }),
            ok: true,
        }]);

        let p = plan_prompt("now add tests for that", &history);
        assert!(p.contains("add a hello function"), "prior goal must appear");
        assert!(p.contains("hello.rs"), "prior edited file must appear");
        assert!(
            p.contains("now add tests for that"),
            "current goal must still appear"
        );
    }

    #[tokio::test]
    async fn falls_back_to_goal_when_unparseable() {
        // LocalProvider echoes the prompt — not JSON — so the planner falls back.
        let router = SingleProviderRouter::new(Arc::new(LocalProvider::new()));
        let milestones = run_planner(&router, "ship it").await;
        assert_eq!(milestones.len(), 1);
        assert_eq!(milestones[0].description, "ship it");
    }
}
