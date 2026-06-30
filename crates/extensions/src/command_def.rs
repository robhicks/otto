//! A discovered `commands/*.md`: optional Claude-Code-compatible frontmatter
//! (`description`, `argument-hint`, `model`, `allowed-tools`) plus a markdown body that is the
//! prompt template. Unlike agents, the command `name` comes from the file path (assigned by the
//! caller), not from frontmatter, and frontmatter is optional — a bare prompt file is valid.

/// One parsed custom command. All frontmatter fields are optional. `model` is preserved for a
/// later slice (not routed). `allowed_tools` is enforced on the `otto run --command` path, where
/// it narrows the tool registry via `ToolRegistry::subset` (absent = all tools, present =
/// intersection, empty = none); it is inert on any other path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomCommandDef {
    pub name: String,
    pub description: Option<String>,
    pub argument_hint: Option<String>,
    pub model: Option<String>,
    pub allowed_tools: Option<Vec<String>>,
    pub template: String,
}

/// Parse one `commands/*.md`. `name` is supplied by the caller (discovery derives it from the
/// path). If `text` starts with `---`, the frontmatter block is split and parsed; otherwise the
/// whole text is the template and every frontmatter field is `None`. An unterminated frontmatter
/// block (`---` with no closing `---`) is an error.
pub fn parse_command_md(name: &str, text: &str) -> anyhow::Result<CustomCommandDef> {
    let mut description = None;
    let mut argument_hint = None;
    let mut model = None;
    let mut allowed_tools = None;

    let template = if let Some(rest) = text.strip_prefix("---") {
        let end = rest
            .find("\n---")
            .ok_or_else(|| anyhow::anyhow!("unterminated frontmatter (no closing `---`)"))?;
        let front = &rest[..end];
        let body = rest[end + 4..].trim_start_matches(['\n', '\r']).to_string();

        for line in front.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let value = value.trim();
            match key.trim() {
                "description" if !value.is_empty() => description = Some(value.to_string()),
                "argument-hint" if !value.is_empty() => argument_hint = Some(value.to_string()),
                "model" if !value.is_empty() => model = Some(value.to_string()),
                // Present (even if empty) → Some(list); only an absent key stays None.
                "allowed-tools" => {
                    let list: Vec<String> = value
                        .trim_matches(['[', ']'])
                        .split(',')
                        .map(|s| s.trim().trim_matches(['"', '\'']).to_string())
                        .filter(|s| !s.is_empty())
                        .collect();
                    allowed_tools = Some(list);
                }
                _ => {}
            }
        }
        body
    } else {
        text.to_string()
    };

    Ok(CustomCommandDef {
        name: name.to_string(),
        description,
        argument_hint,
        model,
        allowed_tools,
        template,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_frontmatter_and_body() {
        let text = "---\ndescription: Commit helper\nargument-hint: <message>\nmodel: claude-opus-4-8\nallowed-tools: bash, fs.read\n---\nCommit with message: $ARGUMENTS\n";
        let def = parse_command_md("git:commit", text).unwrap();
        assert_eq!(def.name, "git:commit");
        assert_eq!(def.description.as_deref(), Some("Commit helper"));
        assert_eq!(def.argument_hint.as_deref(), Some("<message>"));
        assert_eq!(def.model.as_deref(), Some("claude-opus-4-8"));
        assert_eq!(
            def.allowed_tools,
            Some(vec!["bash".to_string(), "fs.read".to_string()])
        );
        assert_eq!(def.template.trim(), "Commit with message: $ARGUMENTS");
    }

    #[test]
    fn allowed_tools_inline_list_form() {
        let text = "---\nallowed-tools: [bash, fs.read]\n---\nbody\n";
        let def = parse_command_md("c", text).unwrap();
        assert_eq!(
            def.allowed_tools,
            Some(vec!["bash".to_string(), "fs.read".to_string()])
        );
    }

    #[test]
    fn no_frontmatter_whole_text_is_template() {
        let text = "Just a prompt with $1 and no frontmatter.\n";
        let def = parse_command_md("plain", text).unwrap();
        assert_eq!(def.description, None);
        assert_eq!(def.argument_hint, None);
        assert_eq!(def.model, None);
        assert_eq!(def.allowed_tools, None);
        assert_eq!(def.template, "Just a prompt with $1 and no frontmatter.\n");
    }

    #[test]
    fn unterminated_frontmatter_errors() {
        let text = "---\ndescription: oops\nno closing fence\n";
        assert!(parse_command_md("c", text).is_err());
    }

    #[test]
    fn unknown_frontmatter_keys_ignored() {
        let text = "---\ndescription: d\nbogus: x\n---\nbody\n";
        let def = parse_command_md("c", text).unwrap();
        assert_eq!(def.description.as_deref(), Some("d"));
        assert_eq!(def.template.trim(), "body");
    }
}
