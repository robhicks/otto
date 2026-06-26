//! Parses a Claude Code plugin manifest (`.claude-plugin/plugin.json`): the plugin `name` plus
//! optional component path overrides. An omitted component field means "use the convention dir"
//! (resolved by discovery, not here). Bundled MCP servers (`mcpServers`) are Plan B and are not
//! parsed in this module yet. Pure parsing — no I/O.

use serde_json::Value;

/// A parsed `plugin.json`. Each `Option<String>` component field is a path **relative to the
/// plugin root**; `None` means discovery falls back to the convention directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginManifest {
    pub name: String,
    pub version: Option<String>,
    pub description: Option<String>,
    pub commands: Option<String>,
    pub agents: Option<String>,
    pub skills: Option<String>,
    pub hooks: Option<String>,
}

/// Parse a `plugin.json` document. Errors on invalid JSON or a missing/empty `name`. Component
/// path overrides are read when present and non-empty; unknown keys (`author`, `homepage`, …) and
/// `mcpServers` (Plan B) are ignored.
pub fn parse_plugin_json(json: &str) -> anyhow::Result<PluginManifest> {
    let v: Value = serde_json::from_str(json)?;
    let name = v
        .get("name")
        .and_then(|n| n.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("plugin.json missing `name`"))?
        .to_string();
    let s = |k: &str| {
        v.get(k)
            .and_then(|x| x.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
    };
    Ok(PluginManifest {
        name,
        version: s("version"),
        description: s("description"),
        commands: s("commands"),
        agents: s("agents"),
        skills: s("skills"),
        hooks: s("hooks"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_name_and_overrides() {
        let json = r#"{
            "name": "foo", "version": "1.0.0", "description": "d",
            "author": { "name": "x" },
            "commands": "./cmds", "agents": "./ag", "skills": "./sk", "hooks": "./h/hooks.json"
        }"#;
        let m = parse_plugin_json(json).unwrap();
        assert_eq!(m.name, "foo");
        assert_eq!(m.version.as_deref(), Some("1.0.0"));
        assert_eq!(m.description.as_deref(), Some("d"));
        assert_eq!(m.commands.as_deref(), Some("./cmds"));
        assert_eq!(m.agents.as_deref(), Some("./ag"));
        assert_eq!(m.skills.as_deref(), Some("./sk"));
        assert_eq!(m.hooks.as_deref(), Some("./h/hooks.json"));
    }

    #[test]
    fn absent_components_are_none() {
        let m = parse_plugin_json(r#"{"name":"foo"}"#).unwrap();
        assert_eq!(m.commands, None);
        assert_eq!(m.agents, None);
        assert_eq!(m.skills, None);
        assert_eq!(m.hooks, None);
        assert_eq!(m.version, None);
    }

    #[test]
    fn missing_name_errors() {
        assert!(parse_plugin_json(r#"{"version":"1.0.0"}"#).is_err());
        assert!(parse_plugin_json(r#"{"name":""}"#).is_err());
    }

    #[test]
    fn malformed_json_errors() {
        assert!(parse_plugin_json("not json").is_err());
    }

    #[test]
    fn mcp_servers_field_is_ignored_this_plan() {
        // Plan A does not parse mcpServers; presence must not break parsing.
        let m = parse_plugin_json(r#"{"name":"foo","mcpServers":{"s":{}}}"#).unwrap();
        assert_eq!(m.name, "foo");
    }
}
