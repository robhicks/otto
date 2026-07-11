/// One styled run within a rendered editor line. `class` maps to a CSS color rule.
#[derive(Clone, Debug, PartialEq)]
pub struct Span {
    pub class: &'static str,
    pub text: String,
}

/// The no-highlight baseline: one plain span per line. The highlight backends replace this with a
/// tokenized version behind the same `(text, lang) -> Vec<Vec<Span>>` shape.
pub fn plain_spans(text: &str) -> Vec<Vec<Span>> {
    text.lines()
        .map(|line| {
            vec![Span {
                class: "tok-plain",
                text: line.to_string(),
            }]
        })
        .collect()
}

/// Split `text` into per-line `Span`s, coalescing runs of equal class. `class_per_byte` is a
/// class-per-source-byte map; the `+1` stride skips the `\n` that `lines()` strips. Pure and
/// target-independent — shared by `highlight_native` (desktop) and the web highlighter (Task 12).
pub fn segment_lines(text: &str, class_per_byte: &[&'static str]) -> Vec<Vec<Span>> {
    let mut out = Vec::new();
    let mut byte = 0usize;
    for line in text.lines() {
        let mut spans: Vec<Span> = Vec::new();
        let line_bytes = line.len();
        let mut i = 0usize;
        while i < line_bytes {
            let class = class_per_byte.get(byte + i).copied().unwrap_or("tok-plain");
            let mut j = i;
            while j < line_bytes
                && class_per_byte.get(byte + j).copied().unwrap_or("tok-plain") == class
            {
                j += 1;
            }
            spans.push(Span {
                class,
                text: line[i..j].to_string(),
            });
            i = j;
        }
        out.push(spans);
        byte += line_bytes + 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_spans_one_run_per_line() {
        let s = plain_spans("a\nbb");
        assert_eq!(s.len(), 2);
        assert_eq!(
            s[0],
            vec![Span {
                class: "tok-plain",
                text: "a".into()
            }]
        );
        assert_eq!(s[1][0].text, "bb");
    }

    #[test]
    fn plain_spans_empty_is_empty() {
        assert!(plain_spans("").is_empty());
    }

    #[test]
    fn segment_lines_coalesces_equal_classes_per_line() {
        // "ab\ncd": bytes 0,1 = keyword; byte 2 = '\n' (skipped); bytes 3,4 = string.
        let per_byte = [
            "tok-keyword",
            "tok-keyword",
            "tok-plain",
            "tok-string",
            "tok-string",
        ];
        let out = segment_lines("ab\ncd", &per_byte);
        assert_eq!(out.len(), 2);
        assert_eq!(
            out[0],
            vec![Span {
                class: "tok-keyword",
                text: "ab".into()
            }]
        );
        assert_eq!(
            out[1],
            vec![Span {
                class: "tok-string",
                text: "cd".into()
            }]
        );
    }
}
