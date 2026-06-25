//! `skill`: a built-in tool that loads a discovered skill's instructions into the current turn.
//! Given `{"skill": "<name>"}` it returns the skill body plus the skill's `resource_dir`, so the
//! agent can read any bundled resource on demand through the gated `fs.read`. The skill name is a
//! registry key (never a filesystem path), so the tool adds no traversal surface of its own; the
//! call carries no `path`/`bash`, so the gate's read-only `Allow` is correct.

use std::collections::HashMap;
use std::path::PathBuf;

use async_trait::async_trait;
use otto_engine_core::tool::Tool;
use serde_json::{Value, json};

use crate::skill_def::CustomSkillDef;

/// Serves discovered skills by name. Built once from the discovered set; holds each skill's
/// instructions and resource directory.
pub struct SkillTool {
    skills: HashMap<String, (String, PathBuf)>,
}

impl SkillTool {
    pub fn new(skills: &[CustomSkillDef]) -> Self {
        let skills = skills
            .iter()
            .map(|s| (s.name.clone(), (s.instructions.clone(), s.root.clone())))
            .collect();
        Self { skills }
    }
}

#[async_trait]
impl Tool for SkillTool {
    fn name(&self) -> &str {
        "skill"
    }

    async fn call(&self, args: Value) -> anyhow::Result<Value> {
        let name = args
            .get("skill")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("skill: missing string `skill`"))?;
        let (instructions, root) = self
            .skills
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("skill: no skill named '{name}'"))?;
        Ok(json!({
            "instructions": instructions,
            "resource_dir": root.to_string_lossy(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def(name: &str, instructions: &str, root: &str) -> CustomSkillDef {
        CustomSkillDef {
            name: name.to_string(),
            description: "d".to_string(),
            allowed_tools: None,
            instructions: instructions.to_string(),
            root: PathBuf::from(root),
        }
    }

    #[tokio::test]
    async fn returns_instructions_and_resource_dir() {
        let tool = SkillTool::new(&[def("greeter", "Say hi.", "/p/.claude/skills/greeter")]);
        let out = tool.call(json!({ "skill": "greeter" })).await.unwrap();
        assert_eq!(out["instructions"], "Say hi.");
        assert_eq!(out["resource_dir"], "/p/.claude/skills/greeter");
    }

    #[tokio::test]
    async fn unknown_skill_errors() {
        let tool = SkillTool::new(&[def("greeter", "x", "/p")]);
        assert!(tool.call(json!({ "skill": "ghost" })).await.is_err());
    }

    #[tokio::test]
    async fn missing_or_non_string_skill_arg_errors() {
        let tool = SkillTool::new(&[def("greeter", "x", "/p")]);
        assert!(tool.call(json!({})).await.is_err());
        assert!(tool.call(json!({ "skill": 7 })).await.is_err());
    }

    #[test]
    fn tool_is_named_skill() {
        let tool = SkillTool::new(&[]);
        assert_eq!(tool.name(), "skill");
    }
}
