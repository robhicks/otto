//! Command template expansion. Two stages: a pure `expand_args` (`$ARGUMENTS`, `$1..$9`) and an
//! async `resolve_injections` that resolves `` !`cmd` `` and `@path` through the gated
//! `ToolRegistry` (so the permission gate, sandbox, and sensitive-path floor all apply).

use otto_engine_core::tool::ToolRegistry;
use serde_json::json;

/// Substitute `$ARGUMENTS` (all args joined by a single space) and `$1`..`$9` (1-based
/// positional; a missing positional becomes the empty string). Pure — runs before injection so a
/// substituted arg may appear inside an injection target (e.g. `@$1`).
///
/// Single left-to-right pass so substituted text is never re-scanned: an argument value that
/// itself contains `$2`/`$ARGUMENTS` is emitted literally. Only `$1`..`$9` are defined — a
/// multi-digit reference like `$10` is left verbatim (the digit run is not a valid placeholder).
pub fn expand_args(template: &str, args: &[String]) -> String {
    let bytes = template.as_bytes();
    let mut out = String::with_capacity(template.len());
    let mut i = 0;
    while i < template.len() {
        // `$` is ASCII, so `i` here is always on a char boundary.
        if bytes[i] == b'$' {
            if template[i..].starts_with("$ARGUMENTS") {
                out.push_str(&args.join(" "));
                i += "$ARGUMENTS".len();
                continue;
            }
            // `$d` where d is 1..9 — but only when not followed by another digit (so `$10`,
            // `$12` stay literal rather than expanding `$1` and leaving a stray digit).
            if i + 1 < template.len() {
                let d = bytes[i + 1];
                let next_is_digit = i + 2 < template.len() && bytes[i + 2].is_ascii_digit();
                if (b'1'..=b'9').contains(&d) && !next_is_digit {
                    let idx = (d - b'0') as usize;
                    out.push_str(args.get(idx - 1).map(String::as_str).unwrap_or(""));
                    i += 2;
                    continue;
                }
            }
        }
        // Copy one whole char (advance by its UTF-8 width to stay on boundaries).
        let ch = template[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Resolve `` !`cmd` `` (gated `bash`) and `@path` (gated `fs.read`) injections. Implemented in
/// Task 3.
pub async fn resolve_injections(text: &str, tools: &ToolRegistry) -> anyhow::Result<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    // `@path` only triggers at the start of input or right after whitespace, so `rob@host`
    // (an email) is never treated as a file reference.
    let mut at_boundary = true;

    while i < chars.len() {
        let c = chars[i];

        // !`cmd` — run via the gated bash tool, inline trimmed stdout.
        if c == '!' && i + 1 < chars.len() && chars[i + 1] == '`' {
            if let Some(close) = (i + 2..chars.len()).find(|&j| chars[j] == '`') {
                let cmd: String = chars[i + 2..close].iter().collect();
                let res = tools
                    .call("bash", json!({ "command": cmd }))
                    .await
                    .map_err(|e| anyhow::anyhow!("command injection `!{cmd}` failed: {e}"))?;
                let stdout = res.get("stdout").and_then(|v| v.as_str()).unwrap_or("");
                out.push_str(stdout.trim_end());
                i = close + 1;
                at_boundary = false;
                continue;
            }
        }

        // @path — read via the gated fs.read tool, inline file contents.
        if c == '@' && at_boundary {
            let mut k = i + 1;
            while k < chars.len() && !chars[k].is_whitespace() && chars[k] != '`' {
                k += 1;
            }
            if k > i + 1 {
                let path: String = chars[i + 1..k].iter().collect();
                let res = tools
                    .call("fs.read", json!({ "path": path }))
                    .await
                    .map_err(|e| anyhow::anyhow!("file injection `@{path}` failed: {e}"))?;
                let content = res.get("content").and_then(|v| v.as_str()).unwrap_or("");
                out.push_str(content);
                i = k;
                at_boundary = false;
                continue;
            }
        }

        out.push(c);
        at_boundary = c.is_whitespace();
        i += 1;
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use otto_engine_core::tool::{Decision, DenyAsk, PermissionGate, Tool};
    use serde_json::Value;
    use std::sync::Arc;

    // ---- expand_args tests (unchanged) ----
    #[test]
    fn substitutes_arguments_and_positionals() {
        let args = vec!["alpha".to_string(), "beta".to_string()];
        assert_eq!(expand_args("all: $ARGUMENTS", &args), "all: alpha beta");
        assert_eq!(
            expand_args("first=$1 second=$2", &args),
            "first=alpha second=beta"
        );
    }

    #[test]
    fn missing_positional_becomes_empty() {
        let args = vec!["only".to_string()];
        assert_eq!(expand_args("[$1][$2][$3]", &args), "[only][][]");
    }

    #[test]
    fn no_args_yields_empty_substitutions() {
        let args: Vec<String> = vec![];
        assert_eq!(expand_args("x=$ARGUMENTS y=$1", &args), "x= y=");
    }

    #[test]
    fn template_without_placeholders_is_verbatim() {
        let args = vec!["ignored".to_string()];
        assert_eq!(expand_args("plain template", &args), "plain template");
    }

    #[test]
    fn multi_digit_placeholder_is_left_literal() {
        // Only $1..$9 are defined; `$10` must NOT be mangled into `<arg1>0`.
        let args = vec!["a".to_string()];
        assert_eq!(expand_args("ref $10 here", &args), "ref $10 here");
        let args2 = vec!["one".to_string(), "two".to_string()];
        assert_eq!(expand_args("$12", &args2), "$12");
    }

    #[test]
    fn arg_value_containing_placeholder_is_not_re_substituted() {
        // A `$N` that came FROM an argument value must be emitted literally, not re-expanded.
        let args = vec!["X$2Y".to_string(), "BETA".to_string()];
        assert_eq!(expand_args("[$1]", &args), "[X$2Y]");
        // Same hazard via $ARGUMENTS (expanded, then must not be re-scanned).
        let args2 = vec!["$1".to_string(), "second".to_string()];
        assert_eq!(expand_args("$ARGUMENTS", &args2), "$1 second");
    }

    // ---- resolve_injections tests ----

    /// `fs.read` stub: echoes the requested path so substitution is observable.
    struct StubRead;
    #[async_trait]
    impl Tool for StubRead {
        fn name(&self) -> &str {
            "fs.read"
        }
        async fn call(&self, args: Value) -> anyhow::Result<Value> {
            let path = args.get("path").and_then(Value::as_str).unwrap_or("");
            Ok(json!({ "content": format!("FILE:{path}") }))
        }
    }

    /// `bash` stub: echoes the command with a trailing newline (to prove trimming).
    struct StubBash;
    #[async_trait]
    impl Tool for StubBash {
        fn name(&self) -> &str {
            "bash"
        }
        async fn call(&self, args: Value) -> anyhow::Result<Value> {
            let cmd = args.get("command").and_then(Value::as_str).unwrap_or("");
            Ok(json!({ "stdout": format!("OUT:{cmd}\n"), "stderr": "", "exit_code": 0 }))
        }
    }

    struct AllowAll;
    impl PermissionGate for AllowAll {
        fn evaluate(&self, _t: &str, _a: &Value) -> Decision {
            Decision::Allow
        }
    }

    /// Denies any call whose `path` arg contains ".env" — a minimal stand-in for the
    /// sensitive-path floor, enough to prove resolve_injections propagates a gate Deny.
    struct DenyEnv;
    impl PermissionGate for DenyEnv {
        fn evaluate(&self, _t: &str, a: &Value) -> Decision {
            let path = a.get("path").and_then(Value::as_str).unwrap_or("");
            if path.contains(".env") {
                Decision::Deny
            } else {
                Decision::Allow
            }
        }
    }

    fn registry(gate: Arc<dyn PermissionGate>, tools: Vec<Arc<dyn Tool>>) -> ToolRegistry {
        let mut reg = ToolRegistry::new(gate, Arc::new(DenyAsk));
        for t in tools {
            reg.register(t);
        }
        reg
    }

    #[tokio::test]
    async fn resolves_file_and_command_injections() {
        let reg = registry(
            Arc::new(AllowAll),
            vec![Arc::new(StubRead), Arc::new(StubBash)],
        );
        let out = resolve_injections("see @src/a.rs then !`echo hi` done", &reg)
            .await
            .unwrap();
        assert!(out.contains("FILE:src/a.rs"), "got: {out}");
        assert!(out.contains("OUT:echo hi"), "got: {out}");
        // Trailing newline from bash stdout is trimmed.
        assert!(!out.contains("OUT:echo hi\n"), "got: {out}");
    }

    #[tokio::test]
    async fn plain_text_is_unchanged() {
        let reg = registry(Arc::new(AllowAll), vec![]);
        assert_eq!(
            resolve_injections("no markers here", &reg).await.unwrap(),
            "no markers here"
        );
    }

    #[tokio::test]
    async fn at_sign_mid_token_is_not_a_file_ref() {
        // `@` after a non-whitespace char (an email) must not trigger injection. No fs.read is
        // registered, so a wrongful match would error — asserting Ok proves it was left alone.
        let reg = registry(Arc::new(AllowAll), vec![]);
        let out = resolve_injections("ping rob@host now", &reg).await.unwrap();
        assert_eq!(out, "ping rob@host now");
    }

    #[tokio::test]
    async fn sensitive_file_injection_fails_closed() {
        let reg = registry(Arc::new(DenyEnv), vec![Arc::new(StubRead)]);
        let err = resolve_injections("secret: @.env", &reg).await;
        assert!(err.is_err(), "expected @.env to be denied");
    }

    #[tokio::test]
    async fn bash_absent_fails_closed() {
        // No `bash` tool registered → !`...` must error, not silently inline empty.
        let reg = registry(Arc::new(AllowAll), vec![Arc::new(StubRead)]);
        let err = resolve_injections("run !`echo hi`", &reg).await;
        assert!(err.is_err(), "expected absent bash to fail closed");
    }

    #[tokio::test]
    async fn adjacent_command_markers_both_resolve() {
        let reg = registry(Arc::new(AllowAll), vec![Arc::new(StubBash)]);
        let out = resolve_injections("!`a`!`b`", &reg).await.unwrap();
        assert_eq!(out, "OUT:aOUT:b");
    }

    #[tokio::test]
    async fn at_path_runs_to_whitespace_consuming_inner_at() {
        // The path token runs until whitespace, so `@a@b` is a single path "a@b".
        let reg = registry(Arc::new(AllowAll), vec![Arc::new(StubRead)]);
        let out = resolve_injections("@a@b", &reg).await.unwrap();
        assert_eq!(out, "FILE:a@b");
    }

    #[tokio::test]
    async fn trailing_bang_and_bare_at_are_literal() {
        // A trailing `!` (no backtick) and a bare `@` at end-of-input resolve to nothing —
        // they are emitted literally, not treated as (failed) injections. No tools needed.
        let reg = registry(Arc::new(AllowAll), vec![]);
        assert_eq!(resolve_injections("done!", &reg).await.unwrap(), "done!");
        assert_eq!(resolve_injections("see @", &reg).await.unwrap(), "see @");
    }

    #[tokio::test]
    async fn at_path_stops_at_backtick() {
        let reg = registry(Arc::new(AllowAll), vec![Arc::new(StubRead)]);
        let out = resolve_injections("@file`x", &reg).await.unwrap();
        assert_eq!(out, "FILE:file`x");
    }
}
