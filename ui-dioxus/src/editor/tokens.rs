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
}
