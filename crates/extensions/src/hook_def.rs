//! Claude-Code `settings.json` hooks. This slice parses the `PreToolUse` and `PostToolUse`
//! events only: each is a list of matcher entries, each entry carrying an optional `matcher`
//! (a tool-name selector) plus one or more `type: "command"` hooks. Other events (SessionStart,
//! Stop, …) parse without error but are not collected. Advanced JSON-stdout control is not parsed
//! here — the runner honors the exit-code contract only.

use serde_json::Value;

/// One `type: "command"` hook: the shell command plus an optional per-hook timeout (seconds).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookCommand {
    pub command: String,
    pub timeout: Option<u64>,
}

/// A matcher entry: which tools it selects (`None`/`""`/`"*"` = all) and the hooks to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookMatcher {
    pub matcher: Option<String>,
    pub hooks: Vec<HookCommand>,
}

/// All discovered tool-dispatch hooks. `Default` is the empty set (no hooks configured).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HookSet {
    pub pre_tool_use: Vec<HookMatcher>,
    pub post_tool_use: Vec<HookMatcher>,
}

impl HookSet {
    /// True when no tool-dispatch hooks are configured.
    pub fn is_empty(&self) -> bool {
        self.pre_tool_use.is_empty() && self.post_tool_use.is_empty()
    }
}

/// Parse a `settings.json` document into its tool-dispatch hooks. A missing `hooks` object (or a
/// settings file with no hooks) yields an empty `HookSet`. Invalid JSON is an error. Individual
/// hook entries that are not `type: "command"` or that lack a non-empty `command` are skipped; a
/// matcher entry left with no runnable commands is dropped.
pub fn parse_hooks(settings_json: &str) -> anyhow::Result<HookSet> {
    let v: Value = serde_json::from_str(settings_json)?;
    let Some(hooks) = v.get("hooks").and_then(|h| h.as_object()) else {
        return Ok(HookSet::default());
    };
    Ok(HookSet {
        pre_tool_use: parse_event(hooks.get("PreToolUse")),
        post_tool_use: parse_event(hooks.get("PostToolUse")),
    })
}

fn parse_event(val: Option<&Value>) -> Vec<HookMatcher> {
    let Some(arr) = val.and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in arr {
        let matcher = entry
            .get("matcher")
            .and_then(|m| m.as_str())
            .map(|s| s.to_string());
        let mut cmds = Vec::new();
        if let Some(hooks) = entry.get("hooks").and_then(|h| h.as_array()) {
            for h in hooks {
                let is_command = h.get("type").and_then(|t| t.as_str()) == Some("command");
                let command = h.get("command").and_then(|c| c.as_str()).unwrap_or("");
                if !is_command || command.is_empty() {
                    continue;
                }
                cmds.push(HookCommand {
                    command: command.to_string(),
                    timeout: h.get("timeout").and_then(|t| t.as_u64()),
                });
            }
        }
        if !cmds.is_empty() {
            out.push(HookMatcher {
                matcher,
                hooks: cmds,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pre_and_post_with_matcher_and_timeout() {
        let json = r#"{
            "hooks": {
                "PreToolUse": [
                    { "matcher": "bash",
                      "hooks": [ { "type": "command", "command": "block.sh", "timeout": 5 } ] }
                ],
                "PostToolUse": [
                    { "hooks": [ { "type": "command", "command": "log.sh" } ] }
                ]
            }
        }"#;
        let set = parse_hooks(json).unwrap();
        assert_eq!(set.pre_tool_use.len(), 1);
        assert_eq!(set.pre_tool_use[0].matcher.as_deref(), Some("bash"));
        assert_eq!(set.pre_tool_use[0].hooks[0].command, "block.sh");
        assert_eq!(set.pre_tool_use[0].hooks[0].timeout, Some(5));
        assert_eq!(set.post_tool_use.len(), 1);
        assert_eq!(set.post_tool_use[0].matcher, None);
        assert_eq!(set.post_tool_use[0].hooks[0].timeout, None);
    }

    #[test]
    fn missing_hooks_object_is_empty_ok() {
        let set = parse_hooks(r#"{ "model": "x" }"#).unwrap();
        assert_eq!(set, HookSet::default());
    }

    #[test]
    fn malformed_json_errors() {
        assert!(parse_hooks("{ not json").is_err());
    }

    #[test]
    fn non_command_and_commandless_hooks_are_skipped() {
        let json = r#"{
            "hooks": { "PreToolUse": [
                { "matcher": "bash", "hooks": [
                    { "type": "other", "command": "x" },
                    { "type": "command" },
                    { "type": "command", "command": "" }
                ] }
            ] }
        }"#;
        let set = parse_hooks(json).unwrap();
        assert!(set.pre_tool_use.is_empty());
    }

    #[test]
    fn unknown_event_keys_ignored() {
        let json = r#"{ "hooks": { "SessionStart": [
            { "hooks": [ { "type": "command", "command": "hi.sh" } ] } ] } }"#;
        let set = parse_hooks(json).unwrap();
        assert_eq!(set, HookSet::default());
    }

    #[test]
    fn is_empty_reflects_presence_of_hooks() {
        assert!(HookSet::default().is_empty());
        let set = parse_hooks(
            r#"{"hooks":{"PreToolUse":[{"hooks":[{"type":"command","command":"x.sh"}]}]}}"#,
        )
        .unwrap();
        assert!(!set.is_empty());
    }
}
