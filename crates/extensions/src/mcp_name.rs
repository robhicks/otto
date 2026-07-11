//! Claude-Code `mcp__` addressing for plugin-bundled MCP tools. otto registers each plugin MCP
//! tool under the internal gate name `plugin__<plugin>__<serverkey>__<tool>`; operators address
//! them with the Claude Code idiom `mcp__<plugin>` (whole plugin) or `mcp__<plugin>__<tool>` (that
//! tool across any of the plugin's servers — the server key is always wildcarded). This bridge is
//! consumed identically by the permission gate (`permission_def`) and hook matchers (`hook_exec`).

/// True if a settings-side specifier addresses the given runtime tool name. Fires ONLY when
/// `specifier` is an `mcp__…` form AND `tool_name` is a `plugin__…` form; returns `false`
/// otherwise so ordinary exact-match handles everything else.
pub fn mcp_specifier_matches(specifier: &str, tool_name: &str) -> bool {
    let Some((plugin, tool)) = parse_plugin_tool(tool_name) else {
        return false;
    };
    let Some((spec_plugin, spec_tool)) = parse_mcp_specifier(specifier) else {
        return false;
    };
    spec_plugin == plugin && spec_tool.is_none_or(|t| t == tool)
}

/// `plugin__<plugin>__<serverkey>__<tool>` → `(plugin, tool)`. The `<tool>` tail is verbatim (it may
/// contain `.`/`_`, e.g. `fs.read`); the server key is discarded (always wildcarded in the `mcp__`
/// form). Wrong prefix, fewer than three segments, or an empty segment → `None`.
fn parse_plugin_tool(name: &str) -> Option<(&str, &str)> {
    let rest = name.strip_prefix("plugin__")?;
    let mut parts = rest.splitn(3, "__");
    let plugin = parts.next().filter(|s| !s.is_empty())?;
    let _serverkey = parts.next().filter(|s| !s.is_empty())?;
    let tool = parts.next().filter(|s| !s.is_empty())?;
    Some((plugin, tool))
}

/// `mcp__<plugin>` → `(plugin, None)`; `mcp__<plugin>__<tool>` → `(plugin, Some(tool))`. The tool
/// tail is verbatim. An empty plugin or tool segment → `None` (a malformed specifier never widens).
fn parse_mcp_specifier(spec: &str) -> Option<(&str, Option<&str>)> {
    let rest = spec.strip_prefix("mcp__")?;
    let (plugin, tool) = match rest.split_once("__") {
        Some((p, t)) => (p, Some(t)),
        None => (rest, None),
    };
    if plugin.is_empty() || tool == Some("") {
        return None;
    }
    Some((plugin, tool))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_level_matches_any_tool_of_that_plugin() {
        assert!(mcp_specifier_matches("mcp__acme", "plugin__acme__srv__search"));
        assert!(mcp_specifier_matches("mcp__acme", "plugin__acme__other__list"));
    }

    #[test]
    fn tool_level_matches_that_tool_across_servers() {
        assert!(mcp_specifier_matches("mcp__acme__search", "plugin__acme__s1__search"));
        assert!(mcp_specifier_matches("mcp__acme__search", "plugin__acme__s2__search"));
        assert!(!mcp_specifier_matches("mcp__acme__search", "plugin__acme__s1__list"));
    }

    #[test]
    fn wrong_plugin_does_not_match() {
        assert!(!mcp_specifier_matches("mcp__acme", "plugin__other__srv__search"));
    }

    #[test]
    fn dotted_tool_tail_is_verbatim() {
        assert!(mcp_specifier_matches(
            "mcp__acme__fs.read",
            "plugin__acme__srv__fs.read"
        ));
    }

    #[test]
    fn non_mcp_specifier_never_matches() {
        assert!(!mcp_specifier_matches("bash", "plugin__acme__srv__search"));
        assert!(!mcp_specifier_matches("fs.read", "plugin__acme__srv__search"));
    }

    #[test]
    fn non_plugin_runtime_name_never_matches() {
        assert!(!mcp_specifier_matches("mcp__acme", "bash"));
        assert!(!mcp_specifier_matches("mcp__acme", "fs.read"));
    }

    #[test]
    fn malformed_specifier_never_widens() {
        assert!(!mcp_specifier_matches("mcp__", "plugin__acme__srv__search"));
        assert!(!mcp_specifier_matches("mcp__acme__", "plugin__acme__srv__search"));
    }

    #[test]
    fn malformed_runtime_name_never_matches() {
        // too few segments after the prefix
        assert!(!mcp_specifier_matches("mcp__acme", "plugin__acme__srv"));
        assert!(!mcp_specifier_matches("mcp__acme", "plugin__acme"));
    }
}
