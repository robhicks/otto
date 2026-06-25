//! A discovered `agents/*.md`: Claude-Code-exact YAML-ish frontmatter + a markdown body
//! that becomes the agent's system prompt.

/// One parsed custom agent. `tools = None` means "all available tools"; `Some(list)` is an
/// allowlist. `model` is preserved for a later slice; it does not influence routing yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomAgentDef {
    pub name: String,
    pub description: String,
    pub tools: Option<Vec<String>>,
    pub model: Option<String>,
    pub system_prompt: String,
}

/// Parse one `agents/*.md`. Errors if the frontmatter is absent/unterminated or is missing
/// `name`/`description`. `tools` accepts a comma-separated string or an inline `[a, b]` list.
pub fn parse_agent_md(text: &str) -> anyhow::Result<CustomAgentDef> {
    let rest = text
        .strip_prefix("---")
        .ok_or_else(|| anyhow::anyhow!("missing frontmatter (no leading `---`)"))?;
    let end = rest
        .find("\n---")
        .ok_or_else(|| anyhow::anyhow!("unterminated frontmatter (no closing `---`)"))?;
    let front = &rest[..end];
    let body = rest[end + 4..].trim_start_matches(['\n', '\r']).to_string();

    let mut name = None;
    let mut description = None;
    let mut tools = None;
    let mut model = None;

    for line in front.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = line
            .split_once(':')
            .ok_or_else(|| anyhow::anyhow!("malformed frontmatter line: {line}"))?;
        let value = value.trim();
        match key.trim() {
            "name" => name = Some(value.to_string()),
            "description" => description = Some(value.to_string()),
            "model" if !value.is_empty() => model = Some(value.to_string()),
            // `tools` PRESENT (even if empty) → `Some(list)`; an empty list means "no tools".
            // Only an ABSENT `tools` key stays `None` (= all tools, Claude-Code-compatible).
            "tools" => {
                let list: Vec<String> = value
                    .trim_matches(['[', ']'])
                    .split(',')
                    .map(|s| s.trim().trim_matches(['"', '\'']).to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                tools = Some(list);
            }
            _ => {}
        }
    }

    let name = name.ok_or_else(|| anyhow::anyhow!("frontmatter missing `name`"))?;
    if name.trim().is_empty() {
        anyhow::bail!("frontmatter `name` is empty");
    }

    Ok(CustomAgentDef {
        name,
        description: description
            .ok_or_else(|| anyhow::anyhow!("frontmatter missing `description`"))?,
        tools,
        model,
        system_prompt: body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "---\nname: reviewer\ndescription: Reviews code\ntools: fs.read, grep\nmodel: claude-opus-4-8\n---\nYou are a careful code reviewer.\n";

    #[test]
    fn parses_frontmatter_and_body() {
        let def = parse_agent_md(SAMPLE).unwrap();
        assert_eq!(def.name, "reviewer");
        assert_eq!(def.description, "Reviews code");
        assert_eq!(
            def.tools,
            Some(vec!["fs.read".to_string(), "grep".to_string()])
        );
        assert_eq!(def.model, Some("claude-opus-4-8".to_string()));
        assert_eq!(def.system_prompt.trim(), "You are a careful code reviewer.");
    }

    #[test]
    fn tools_inline_list_form() {
        let text = "---\nname: a\ndescription: d\ntools: [fs.read, bash]\n---\nbody\n";
        let def = parse_agent_md(text).unwrap();
        assert_eq!(
            def.tools,
            Some(vec!["fs.read".to_string(), "bash".to_string()])
        );
    }

    #[test]
    fn omitted_tools_and_model_are_none() {
        let text = "---\nname: a\ndescription: d\n---\nbody\n";
        let def = parse_agent_md(text).unwrap();
        assert_eq!(def.tools, None);
        assert_eq!(def.model, None);
    }

    #[test]
    fn missing_name_errors() {
        let text = "---\ndescription: d\n---\nbody\n";
        assert!(parse_agent_md(text).is_err());
    }

    #[test]
    fn missing_frontmatter_errors() {
        assert!(parse_agent_md("just a body, no frontmatter").is_err());
    }

    #[test]
    fn blank_name_errors() {
        let text = "---\nname:    \ndescription: d\n---\nbody\n";
        assert!(parse_agent_md(text).is_err());
    }

    #[test]
    fn empty_tools_value_means_no_tools_not_all() {
        // Present-but-empty `tools:` must parse to Some(empty), NOT None (which = all tools).
        let text = "---\nname: a\ndescription: d\ntools:\n---\nbody\n";
        let def = parse_agent_md(text).unwrap();
        assert_eq!(def.tools, Some(vec![]));
    }

    #[test]
    fn empty_inline_list_tools_means_no_tools() {
        let text = "---\nname: a\ndescription: d\ntools: []\n---\nbody\n";
        let def = parse_agent_md(text).unwrap();
        assert_eq!(def.tools, Some(vec![]));
    }

    #[test]
    fn comment_lines_in_frontmatter_are_skipped() {
        let text = "---\n# this is a comment\nname: a\ndescription: d\n---\nbody\n";
        let def = parse_agent_md(text).unwrap();
        assert_eq!(def.name, "a");
        assert_eq!(def.description, "d");
    }
}
