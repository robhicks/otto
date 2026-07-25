//! Desktop-native syntax highlighting via tree-sitter. Compiled only under `--features desktop`;
//! the web build uses `highlight_web`'s lexer instead, because tree-sitter's C grammars cannot be
//! built for `wasm32-unknown-unknown`. Both backends cover the same five languages and emit the
//! same `tokens::VOCAB` classes, so they differ in fidelity rather than capability — see
//! `docs/superpowers/specs/2026-07-24-ui-dioxus-web-highlighting-design.md`.
//!
//! `highlight` MUST fall back to `plain_spans` on any failure (unsupported language, a query/parse
//! error, anything) — highlighting is best-effort and must never break the editor.

// Class constants come from `tokens`, shared with `highlight_web`, so the two backends cannot
// drift apart on the vocabulary that `style.css` styles.
use crate::editor::tokens::{
    plain_spans, segment_lines, Span, COMMENT, KEYWORD, NUMBER, PLAIN, STRING, TYPE,
};

/// Map a `language_for_path` id to its loaded grammar + highlights query; `None` => no
/// highlighting (unsupported language — caller falls back to `plain_spans`). Mirrors the
/// language set `crates/retrieval` vendors (Rust/JS/TS/Python/Go).
fn language(lang: &str) -> Option<(tree_sitter::Language, &'static str)> {
    Some(match lang {
        "rust" => (
            tree_sitter_rust::LANGUAGE.into(),
            tree_sitter_rust::HIGHLIGHTS_QUERY,
        ),
        "javascript" => (
            tree_sitter_javascript::LANGUAGE.into(),
            tree_sitter_javascript::HIGHLIGHT_QUERY,
        ),
        "typescript" => (
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            tree_sitter_typescript::HIGHLIGHTS_QUERY,
        ),
        "python" => (
            tree_sitter_python::LANGUAGE.into(),
            tree_sitter_python::HIGHLIGHTS_QUERY,
        ),
        "go" => (
            tree_sitter_go::LANGUAGE.into(),
            tree_sitter_go::HIGHLIGHTS_QUERY,
        ),
        _ => return None,
    })
}

/// Highlight-capture names we style; the index into this array is the `Highlight.0` the events
/// carry. `class_for` maps each back to a CSS class from Task 10 (`style.css`'s `tok-*` rules).
const CAPTURES: [&str; 5] = ["keyword", "string", "comment", "type", "number"];

fn class_for(idx: usize) -> &'static str {
    match CAPTURES.get(idx).copied().unwrap_or("") {
        "keyword" => KEYWORD,
        "string" => STRING,
        "comment" => COMMENT,
        "type" => TYPE,
        "number" => NUMBER,
        _ => PLAIN,
    }
}

/// Highlight `text` for `lang`. Falls back to `plain_spans` for unsupported langs or any parse /
/// query failure (highlighting is best-effort; the editor must never break on it).
pub fn highlight(text: &str, lang: &str) -> Vec<Vec<Span>> {
    match language(lang).and_then(|(language, query)| highlight_inner(text, language, query)) {
        Some(spans) => spans,
        None => plain_spans(text),
    }
}

fn highlight_inner(
    text: &str,
    language: tree_sitter::Language,
    query: &str,
) -> Option<Vec<Vec<Span>>> {
    use tree_sitter_highlight::{HighlightConfiguration, HighlightEvent, Highlighter};
    let mut cfg = HighlightConfiguration::new(language, "editor", query, "", "").ok()?;
    cfg.configure(&CAPTURES);
    let mut hl = Highlighter::new();
    let mut cur = PLAIN;
    let mut class_per_byte = vec![PLAIN; text.len()];
    let events = hl.highlight(&cfg, text.as_bytes(), None, |_| None).ok()?;
    for ev in events {
        match ev.ok()? {
            HighlightEvent::HighlightStart(h) => cur = class_for(h.0),
            HighlightEvent::HighlightEnd => cur = PLAIN,
            HighlightEvent::Source { start, end } => {
                for b in start..end.min(class_per_byte.len()) {
                    class_per_byte[b] = cur;
                }
            }
        }
    }
    Some(segment_lines(text, &class_per_byte))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlight_unsupported_lang_falls_back_to_plain() {
        let out = highlight("hello world", "text");
        assert_eq!(out, plain_spans("hello world"));
    }

    #[test]
    fn highlight_rust_classifies_a_keyword() {
        let out = highlight("fn main() {}", "rust");
        let found = out.iter().flatten().any(|sp| sp.class == "tok-keyword");
        assert!(found, "expected a tok-keyword span, got {out:?}");
    }

    /// Pins this backend's exact output so adding the web highlighter (or any later change to the
    /// shared `segment_lines` seam) cannot silently alter what desktop renders. If tree-sitter's
    /// grammars/queries are upgraded this may legitimately need re-pinning — but it should never
    /// change as a side effect of work on the other target.
    ///
    /// Note what this pins: `1` is `tok-plain`, NOT `tok-number`. `tree_sitter_rust`'s
    /// `HIGHLIGHTS_QUERY` has no `number` capture, so integer literals go unclassified on desktop
    /// for Rust. The web lexer does classify them, so the two targets genuinely differ here — a
    /// fidelity difference in web's favour on this one point. Recorded rather than papered over.
    #[test]
    fn desktop_output_is_pinned_for_a_fixed_input() {
        let out = highlight("// c\nfn f() -> u32 { 1 }", "rust");
        let rendered: Vec<Vec<(&str, String)>> = out
            .iter()
            .map(|line| {
                line.iter()
                    .map(|s| (s.class, s.text.clone()))
                    .collect::<Vec<_>>()
            })
            .collect();
        assert_eq!(
            rendered,
            vec![
                vec![("tok-comment", "// c".to_string())],
                vec![
                    ("tok-keyword", "fn".to_string()),
                    ("tok-plain", " f() -> ".to_string()),
                    ("tok-type", "u32".to_string()),
                    ("tok-plain", " { 1 }".to_string()),
                ],
            ]
        );
    }

    /// The real cross-backend contract, exercised by running BOTH highlighters over the same input
    /// (possible here because `highlight_web` is compiled under `cfg(test)` on every target).
    ///
    /// An earlier version of this test hardcoded its own copy of the class list and only called the
    /// native backend, so it could not fail for any input — adding a class to one backend would
    /// have left it green. It now checks against the single `tokens::VOCAB` both backends import,
    /// and asserts the two properties that actually make one `style.css` serve both targets:
    /// neither backend invents a class, and both segment the source into the same lines with the
    /// same text (only the colours may differ).
    #[test]
    fn both_backends_agree_on_vocabulary_and_line_structure() {
        use crate::editor::highlight_web;
        use crate::editor::tokens::VOCAB;

        let src = "// c\nfn f(x: u32) -> String { let s = \"héllo\"; 1 }\r\nlet t = 'a';";
        for lang in ["rust", "javascript", "typescript", "python", "go"] {
            let native = highlight(src, lang);
            let web = highlight_web::highlight(src, lang);
            for (backend, out) in [("native", &native), ("web", &web)] {
                for span in out.iter().flatten() {
                    assert!(
                        VOCAB.contains(&span.class),
                        "{backend}/{lang}: class {:?} is not in tokens::VOCAB, so style.css \
                         has no rule for it",
                        span.class
                    );
                }
            }
            let text_of = |out: &Vec<Vec<Span>>| -> Vec<String> {
                out.iter()
                    .map(|l| l.iter().map(|s| s.text.as_str()).collect())
                    .collect()
            };
            assert_eq!(
                text_of(&native),
                text_of(&web),
                "{lang}: backends disagree on line structure — the editor overlays a textarea on \
                 the highlight layer, so differing line counts misalign the caret"
            );
        }
    }
}
