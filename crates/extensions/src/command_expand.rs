//! Command template expansion. Two stages: a pure `expand_args` (`$ARGUMENTS`, `$1..$9`) and an
//! async `resolve_injections` that resolves `` !`cmd` `` and `@path` through the gated
//! `ToolRegistry` (so the permission gate, sandbox, and sensitive-path floor all apply).

use otto_engine_core::tool::ToolRegistry;
use serde_json::json;

/// Substitute `$ARGUMENTS` (all args joined by a single space) and `$1`..`$9` (1-based
/// positional; a missing positional becomes the empty string). Pure — runs before injection so a
/// substituted arg may appear inside an injection target (e.g. `@$1`).
pub fn expand_args(template: &str, args: &[String]) -> String {
    let mut out = template.replace("$ARGUMENTS", &args.join(" "));
    for i in 1..=9usize {
        let val = args.get(i - 1).map(String::as_str).unwrap_or("");
        out = out.replace(&format!("${i}"), val);
    }
    out
}

/// Resolve `` !`cmd` `` (gated `bash`) and `@path` (gated `fs.read`) injections. Implemented in
/// Task 3.
pub async fn resolve_injections(text: &str, tools: &ToolRegistry) -> anyhow::Result<String> {
    let _ = (text, tools, json!(null));
    anyhow::bail!("not implemented")
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
