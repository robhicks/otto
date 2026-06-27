//! Parses a Claude Code plugin manifest (`.claude-plugin/plugin.json`): the plugin `name`, optional
//! component path overrides, and the bundled-MCP-server declaration (`mcpServers`: a path to a JSON
//! file or an inline object). An omitted component field means "use the convention dir" (resolved by
//! discovery, not here). Pure parsing — no I/O; `${CLAUDE_PLUGIN_ROOT}` expansion happens at fold
//! time, where the plugin root is known.

use serde_json::Value;
use std::collections::BTreeMap;

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
    pub mcp_servers: Option<McpServersField>,
}

/// A bundled MCP server config, resolved to pure data (no process spawned here). `command`/`args`/
/// `env`/`cwd` are stored verbatim from the manifest; `${CLAUDE_PLUGIN_ROOT}` expansion happens at
/// fold time (in `lib.rs`, where the plugin root is known).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginMcpServer {
    pub namespace: String,  // the plugin name, for tool-name prefixing
    pub server_key: String, // the key under "mcpServers"
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: Option<String>,
}

/// How a plugin declares its MCP servers: a path to a JSON file (relative to the plugin root) or an
/// inline object (the value of the `mcpServers` key in `plugin.json`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpServersField {
    Path(String),
    Inline(Value),
}

/// Parse a `plugin.json` document. Errors on invalid JSON or a missing/empty `name`. Component
/// path overrides are read when present and non-empty; the `mcpServers` field is parsed into an
/// `McpServersField` (path-or-inline); unknown keys (`author`, `homepage`, …) are ignored.
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
    let mcp_servers = match v.get("mcpServers") {
        Some(Value::String(s)) if !s.is_empty() => Some(McpServersField::Path(s.clone())),
        Some(Value::Object(o)) => Some(McpServersField::Inline(Value::Object(o.clone()))),
        _ => None,
    };
    Ok(PluginManifest {
        name,
        version: s("version"),
        description: s("description"),
        commands: s("commands"),
        agents: s("agents"),
        skills: s("skills"),
        hooks: s("hooks"),
        mcp_servers,
    })
}

/// Parse a map of `server_key -> config` into `PluginMcpServer` specs, namespaced by `namespace`.
/// A server missing a non-empty `command` is skipped. `args` defaults to empty, `env` to empty,
/// `cwd` to `None`. Values are stored verbatim — `${CLAUDE_PLUGIN_ROOT}` is expanded by the caller.
pub fn parse_mcp_servers(servers: &Value, namespace: &str) -> Vec<PluginMcpServer> {
    let Some(map) = servers.as_object() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (server_key, cfg) in map {
        let Some(command) = cfg
            .get("command")
            .and_then(|c| c.as_str())
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        let args = cfg
            .get("args")
            .and_then(|a| a.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let env = cfg
            .get("env")
            .and_then(|e| e.as_object())
            .map(|o| {
                o.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default();
        let cwd = cfg
            .get("cwd")
            .and_then(|c| c.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        out.push(PluginMcpServer {
            namespace: namespace.to_string(),
            server_key: server_key.clone(),
            command: command.to_string(),
            args,
            env,
            cwd,
        });
    }
    out
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
    fn parses_mcp_servers_path_field() {
        let m = parse_plugin_json(r#"{"name":"foo","mcpServers":"./.mcp.json"}"#).unwrap();
        assert_eq!(
            m.mcp_servers,
            Some(McpServersField::Path("./.mcp.json".to_string()))
        );
    }

    #[test]
    fn parses_mcp_servers_inline_field() {
        let m =
            parse_plugin_json(r#"{"name":"foo","mcpServers":{"s":{"command":"node"}}}"#).unwrap();
        assert!(matches!(m.mcp_servers, Some(McpServersField::Inline(_))));
    }

    #[test]
    fn absent_mcp_servers_is_none() {
        let m = parse_plugin_json(r#"{"name":"foo"}"#).unwrap();
        assert_eq!(m.mcp_servers, None);
    }

    #[test]
    fn parse_mcp_servers_maps_each_server() {
        // The map of server_key -> config (the value under "mcpServers").
        let v: serde_json::Value = serde_json::from_str(
            r#"{"my-server":{"command":"node","args":["${CLAUDE_PLUGIN_ROOT}/s.js","--x"],
                 "env":{"FOO":"bar"},"cwd":"${CLAUDE_PLUGIN_ROOT}"}}"#,
        )
        .unwrap();
        let specs = parse_mcp_servers(&v, "foo");
        assert_eq!(specs.len(), 1);
        let s = &specs[0];
        assert_eq!(s.namespace, "foo");
        assert_eq!(s.server_key, "my-server");
        assert_eq!(s.command, "node");
        assert_eq!(s.args, vec!["${CLAUDE_PLUGIN_ROOT}/s.js", "--x"]); // un-expanded here; fold expands
        assert_eq!(s.env.get("FOO").map(String::as_str), Some("bar"));
        assert_eq!(s.cwd.as_deref(), Some("${CLAUDE_PLUGIN_ROOT}"));
    }

    #[test]
    fn parse_mcp_servers_skips_server_without_command() {
        let v: serde_json::Value =
            serde_json::from_str(r#"{"good":{"command":"node"},"bad":{"args":["x"]}}"#).unwrap();
        let specs = parse_mcp_servers(&v, "foo");
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].server_key, "good");
    }

    #[test]
    fn parse_mcp_servers_drops_non_string_args_and_env() {
        // By design, values are taken as strings: non-string args/env entries are silently dropped.
        let v: serde_json::Value = serde_json::from_str(
            r#"{"s":{"command":"node","args":[1,"good",null],
                 "env":{"OK":"yes","NUM":7}}}"#,
        )
        .unwrap();
        let s = &parse_mcp_servers(&v, "ns")[0];
        assert_eq!(s.args, vec!["good"]);
        assert_eq!(s.env.get("OK").map(String::as_str), Some("yes"));
        assert_eq!(s.env.get("NUM"), None, "non-string env value is dropped");
    }

    #[test]
    fn parse_mcp_servers_defaults_args_env_cwd() {
        let v: serde_json::Value = serde_json::from_str(r#"{"s":{"command":"x"}}"#).unwrap();
        let s = &parse_mcp_servers(&v, "ns")[0];
        assert!(s.args.is_empty());
        assert!(s.env.is_empty());
        assert_eq!(s.cwd, None);
    }
}
