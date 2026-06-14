//! `ScriptedProvider`: a deterministic provider that returns canned responses keyed by a
//! substring of the prompt. For tests and demos of LLM-dependent code (agents that
//! prompt-and-parse). Like `LocalProvider`, it performs no network I/O.

use async_trait::async_trait;
use otto_engine_core::traits::Provider;
use otto_engine_core::types::{CompleteRequest, CompleteResponse};

/// Returns the first rule whose `needle` is found in the prompt, else `default`.
pub struct ScriptedProvider {
    rules: Vec<(String, String)>,
    default: String,
}

impl ScriptedProvider {
    pub fn new(default: impl Into<String>) -> Self {
        Self {
            rules: Vec::new(),
            default: default.into(),
        }
    }

    /// Add a rule: if the prompt contains `needle`, return `response`. First match wins.
    pub fn on(mut self, needle: impl Into<String>, response: impl Into<String>) -> Self {
        self.rules.push((needle.into(), response.into()));
        self
    }
}

#[async_trait]
impl Provider for ScriptedProvider {
    fn id(&self) -> &str {
        "scripted"
    }

    async fn complete(&self, req: CompleteRequest) -> anyhow::Result<CompleteResponse> {
        let text = self
            .rules
            .iter()
            .find(|(needle, _)| req.prompt.contains(needle.as_str()))
            .map(|(_, resp)| resp.clone())
            .unwrap_or_else(|| self.default.clone());
        Ok(CompleteResponse { text })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn returns_matching_rule_then_default() {
        let p = ScriptedProvider::new("DEFAULT")
            .on("edits", "CODE")
            .on("milestones", "PLAN");

        let code = p
            .complete(CompleteRequest {
                prompt: "give me edits".into(),
            })
            .await
            .unwrap();
        assert_eq!(code.text, "CODE");

        let plan = p
            .complete(CompleteRequest {
                prompt: "give me milestones".into(),
            })
            .await
            .unwrap();
        assert_eq!(plan.text, "PLAN");

        let other = p
            .complete(CompleteRequest {
                prompt: "hello".into(),
            })
            .await
            .unwrap();
        assert_eq!(other.text, "DEFAULT");
        assert_eq!(p.id(), "scripted");
    }
}
