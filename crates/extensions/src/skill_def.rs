//! A discovered `skills/<name>/SKILL.md`: Claude-Code-compatible frontmatter
//! (`name`/`description`/`allowed-tools`) plus a markdown body that is the skill's instructions.
//! `description` is REQUIRED (Claude Code uses it to decide when a skill applies); a skill without
//! one is unusable and rejected here so discovery skips it rather than load empty guidance.
//! `allowed_tools` is parsed and preserved but inert this slice — otto's gate stays the sole
//! authority. `root` (the skill directory, used for resource lookup) is assigned by discovery,
//! not parsed.

use std::path::PathBuf;

/// One parsed skill. `name` defaults to the skill directory name (supplied by discovery); a
/// frontmatter `name` overrides it. `root` is filled in by discovery (empty until then).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomSkillDef {
    pub name: String,
    pub description: String,
    pub allowed_tools: Option<Vec<String>>,
    pub instructions: String,
    pub root: PathBuf,
}

/// Parse one `SKILL.md`. `name` is the fallback name (discovery derives it from the directory); a
/// frontmatter `name` overrides it. The body after the frontmatter is the instructions. A missing
/// or empty `description`, absent frontmatter, or an unterminated frontmatter fence is an error.
pub fn parse_skill_md(name: &str, text: &str) -> anyhow::Result<CustomSkillDef> {
    let rest = text
        .strip_prefix("---")
        .ok_or_else(|| anyhow::anyhow!("SKILL.md has no frontmatter (missing `description`)"))?;
    let end = rest
        .find("\n---")
        .ok_or_else(|| anyhow::anyhow!("unterminated frontmatter (no closing `---`)"))?;
    let front = &rest[..end];
    let instructions = rest[end + 4..].trim_start_matches(['\n', '\r']).to_string();

    let mut fm_name = None;
    let mut description = None;
    let mut allowed_tools = None;
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
            "name" if !value.is_empty() => fm_name = Some(value.to_string()),
            "description" if !value.is_empty() => description = Some(value.to_string()),
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

    let description = description
        .ok_or_else(|| anyhow::anyhow!("SKILL.md is missing a non-empty `description`"))?;

    Ok(CustomSkillDef {
        name: fm_name.unwrap_or_else(|| name.to_string()),
        description,
        allowed_tools,
        instructions,
        root: PathBuf::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_frontmatter_and_body() {
        let text = "---\nname: pdf\ndescription: Fill PDFs\nallowed-tools: bash, fs.read\n---\nUse the form.\n";
        let def = parse_skill_md("dir-name", text).unwrap();
        assert_eq!(def.name, "pdf", "frontmatter name overrides the dir name");
        assert_eq!(def.description, "Fill PDFs");
        assert_eq!(
            def.allowed_tools,
            Some(vec!["bash".to_string(), "fs.read".to_string()])
        );
        assert_eq!(def.instructions.trim(), "Use the form.");
        assert_eq!(
            def.root,
            PathBuf::new(),
            "root is assigned by discovery, not parsing"
        );
    }

    #[test]
    fn allowed_tools_inline_list_form() {
        let text = "---\ndescription: d\nallowed-tools: [bash, fs.read]\n---\nbody\n";
        let def = parse_skill_md("s", text).unwrap();
        assert_eq!(
            def.allowed_tools,
            Some(vec!["bash".to_string(), "fs.read".to_string()])
        );
    }

    #[test]
    fn name_falls_back_to_supplied_when_frontmatter_omits_it() {
        let text = "---\ndescription: d\n---\nbody\n";
        let def = parse_skill_md("dir-name", text).unwrap();
        assert_eq!(def.name, "dir-name");
        assert_eq!(def.allowed_tools, None);
    }

    #[test]
    fn missing_description_errors() {
        let text = "---\nname: s\n---\nbody\n";
        assert!(parse_skill_md("s", text).is_err());
    }

    #[test]
    fn empty_description_errors() {
        let text = "---\ndescription:\n---\nbody\n";
        assert!(parse_skill_md("s", text).is_err());
    }

    #[test]
    fn no_frontmatter_errors() {
        let text = "Just a body, no frontmatter and so no description.\n";
        assert!(parse_skill_md("s", text).is_err());
    }

    #[test]
    fn unterminated_frontmatter_errors() {
        let text = "---\ndescription: oops\nno closing fence\n";
        assert!(parse_skill_md("s", text).is_err());
    }

    #[test]
    fn unknown_frontmatter_keys_ignored() {
        let text = "---\ndescription: d\nbogus: x\n---\nbody\n";
        let def = parse_skill_md("s", text).unwrap();
        assert_eq!(def.description, "d");
        assert_eq!(def.instructions.trim(), "body");
    }
}
