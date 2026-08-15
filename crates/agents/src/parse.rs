//! Extract a JSON value from an LLM completion: tolerates ```json ... ``` fences and
//! surrounding prose by slicing from the first `{` to the last `}` as a fallback.

use serde::de::DeserializeOwned;

use otto_engine_core::types::SessionHistory;

/// Render prior turns for a prompt. Returns an **empty string** when there is no history, so
/// a first turn's prompt is byte-identical to the pre-history prompt — the invariant the
/// offline suite pins and depends on.
pub(crate) fn history_block(history: &SessionHistory) -> String {
    if history.is_empty() {
        return String::new();
    }
    let mut s = String::from("\nEarlier in this session:\n");
    for t in history.turns() {
        s.push_str(&format!("- Goal: {}\n", t.goal));
        if !t.milestones.is_empty() {
            s.push_str(&format!("  Planned: {}\n", t.milestones.join("; ")));
        }
        if !t.files_edited.is_empty() {
            let files: Vec<String> = t
                .files_edited
                .iter()
                .map(|p| p.display().to_string())
                .collect();
            s.push_str(&format!("  Edited: {}\n", files.join(", ")));
        }
        if let Some(v) = &t.verify {
            s.push_str(&format!(
                "  Verify: {}\n",
                if v.ok { "passed" } else { "failed" }
            ));
        }
    }
    s
}

/// Parse `T` from `text`, tolerating Markdown code fences and leading/trailing prose.
pub fn extract_json<T: DeserializeOwned>(text: &str) -> anyhow::Result<T> {
    let slice =
        json_slice(text).ok_or_else(|| anyhow::anyhow!("no JSON object found in completion"))?;
    Ok(serde_json::from_str(slice)?)
}

/// Find the substring that looks like the JSON body: prefer the content of a fenced
/// triple-backtick block, else the span from the first `{` to the last `}`.
fn json_slice(text: &str) -> Option<&str> {
    if let Some(fence_start) = text.find("```") {
        let after = &text[fence_start + 3..];
        // Skip the rest of the fence line (e.g. "json").
        if let Some(nl) = after.find('\n') {
            let body = &after[nl + 1..];
            if let Some(end) = body.find("```") {
                let inner = body[..end].trim();
                if !inner.is_empty() {
                    return Some(inner);
                }
            }
        }
    }
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    if end > start {
        Some(&text[start..=end])
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Deserialize, PartialEq, Debug)]
    struct Demo {
        x: u32,
    }

    #[test]
    fn parses_plain_json() {
        let d: Demo = extract_json("{\"x\": 5}").unwrap();
        assert_eq!(d, Demo { x: 5 });
    }

    #[test]
    fn parses_json_in_fence_with_prose() {
        let text = "Sure!\n```json\n{\"x\": 7}\n```\nDone.";
        let d: Demo = extract_json(text).unwrap();
        assert_eq!(d, Demo { x: 7 });
    }

    #[test]
    fn parses_json_with_surrounding_prose_no_fence() {
        let d: Demo = extract_json("Here: {\"x\": 9} ok").unwrap();
        assert_eq!(d, Demo { x: 9 });
    }

    #[test]
    fn errors_when_no_json() {
        let r: anyhow::Result<Demo> = extract_json("no json here");
        assert!(r.is_err());
    }
}
