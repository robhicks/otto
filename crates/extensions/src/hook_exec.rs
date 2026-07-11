//! Hook matching + the execution seam. `extensions` stays hermetic by depending only on a
//! `HookExecutor` trait; the engine binary supplies the sandboxed implementation. Matching is
//! intentionally simple this slice: `None`/`""`/`"*"` selects every tool, otherwise the matcher
//! is a `|`-separated list of exact tool names; tokens beginning `mcp__` resolve against
//! plugin-bundled MCP tool names via `mcp_specifier_matches` (regex is future work).

use std::time::Duration;

use async_trait::async_trait;

use crate::hook_def::{HookCommand, HookSet};

/// Which tool-dispatch lifecycle point a hook fires at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookEvent {
    PreToolUse,
    PostToolUse,
}

/// The result of running one hook command. `exit_code` is `None` if the process was killed
/// (e.g. by a signal) rather than exiting normally.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookOutcome {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// Runs a single hook command, piping `stdin_json` to its stdin, killed after `timeout`. The
/// engine supplies a sandboxed implementation; tests supply a fake. An `Err` means the command
/// could not be run (no backend, spawn failure, timeout) — the caller treats that as
/// non-blocking.
#[async_trait]
pub trait HookExecutor: Send + Sync {
    async fn run(
        &self,
        command: &str,
        stdin_json: &str,
        timeout: Duration,
    ) -> anyhow::Result<HookOutcome>;
}

/// Does `matcher` select `tool_name`? `None`/`""`/`"*"` match everything; otherwise the matcher is
/// split on `|` and each trimmed token compared: an `mcp__…` token resolves against a plugin MCP
/// tool name via `mcp_name::mcp_specifier_matches`, every other token by exact equality.
pub fn matcher_selects(matcher: &Option<String>, tool_name: &str) -> bool {
    match matcher.as_deref() {
        None | Some("") | Some("*") => true,
        Some(pat) => pat.split('|').any(|t| {
            let t = t.trim();
            // An `mcp__…` token addresses a plugin-bundled MCP tool via the shared bridge; every
            // other token is an exact tool-name match (regex matchers are a later slice).
            if t.starts_with("mcp__") {
                crate::mcp_name::mcp_specifier_matches(t, tool_name)
            } else {
                t == tool_name
            }
        }),
    }
}

impl HookSet {
    /// The hook commands that should fire for `event` on `tool_name`, in declaration order
    /// (user-base entries first, then project — see discovery).
    pub fn matched(&self, event: HookEvent, tool_name: &str) -> Vec<HookCommand> {
        let matchers = match event {
            HookEvent::PreToolUse => &self.pre_tool_use,
            HookEvent::PostToolUse => &self.post_tool_use,
        };
        matchers
            .iter()
            .filter(|m| matcher_selects(&m.matcher, tool_name))
            .flat_map(|m| m.hooks.iter().cloned())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hook_def::{HookMatcher, HookSet};

    fn cmd(c: &str) -> HookCommand {
        HookCommand {
            command: c.to_string(),
            timeout: None,
        }
    }

    #[test]
    fn matcher_wildcard_and_none_match_all() {
        assert!(matcher_selects(&None, "bash"));
        assert!(matcher_selects(&Some("".to_string()), "bash"));
        assert!(matcher_selects(&Some("*".to_string()), "bash"));
    }

    #[test]
    fn matcher_exact_and_alternation() {
        assert!(matcher_selects(&Some("bash".to_string()), "bash"));
        assert!(!matcher_selects(&Some("bash".to_string()), "fs.read"));
        assert!(matcher_selects(
            &Some("bash | fs.read".to_string()),
            "fs.read"
        ));
        assert!(!matcher_selects(&Some("bash|grep".to_string()), "fs.write"));
    }

    #[test]
    fn matched_collects_in_order_for_event_and_tool() {
        let set = HookSet {
            pre_tool_use: vec![
                HookMatcher {
                    matcher: Some("bash".to_string()),
                    hooks: vec![cmd("a")],
                },
                HookMatcher {
                    matcher: None,
                    hooks: vec![cmd("b")],
                },
                HookMatcher {
                    matcher: Some("grep".to_string()),
                    hooks: vec![cmd("c")],
                },
            ],
            post_tool_use: vec![HookMatcher {
                matcher: None,
                hooks: vec![cmd("d")],
            }],
        };
        let pre: Vec<_> = set
            .matched(HookEvent::PreToolUse, "bash")
            .into_iter()
            .map(|h| h.command)
            .collect();
        assert_eq!(pre, vec!["a", "b"]);
        let post: Vec<_> = set
            .matched(HookEvent::PostToolUse, "bash")
            .into_iter()
            .map(|h| h.command)
            .collect();
        assert_eq!(post, vec!["d"]);
    }

    #[test]
    fn mcp_matcher_selects_plugin_tool() {
        assert!(matcher_selects(
            &Some("mcp__acme".to_string()),
            "plugin__acme__srv__search"
        ));
        assert!(matcher_selects(
            &Some("mcp__acme__search".to_string()),
            "plugin__acme__s2__search"
        ));
        assert!(!matcher_selects(
            &Some("mcp__acme".to_string()),
            "plugin__other__srv__search"
        ));
        assert!(!matcher_selects(&Some("mcp__acme".to_string()), "bash"));
    }

    #[test]
    fn mcp_matcher_in_alternation() {
        assert!(matcher_selects(
            &Some("bash|mcp__acme".to_string()),
            "plugin__acme__srv__x"
        ));
        assert!(matcher_selects(&Some("bash|mcp__acme".to_string()), "bash"));
    }
}
