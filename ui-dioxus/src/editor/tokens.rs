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
/// class-per-source-byte map (one class per source byte). Pure and target-independent — shared by
/// `highlight_native` (desktop) and the web highlighter (Task 12).
///
/// Byte tracking is EXACT and cuts are char-boundary-safe: we iterate `split_inclusive('\n')` (the
/// terminator is kept, so advancing `byte` by the returned slice's full length is exact for both
/// `\n` and `\r\n` — unlike `lines()` + a `+1` stride, which under-counts by one per CRLF line and
/// then reads the class map shifted). Runs are grouped over whole CHARS via `char_indices`, so a
/// class boundary can never fall inside a multibyte char and `content[..]` slicing never panics.
pub fn segment_lines(text: &str, class_per_byte: &[&'static str]) -> Vec<Vec<Span>> {
    let mut out = Vec::new();
    let mut byte = 0usize;
    for line_with_term in text.split_inclusive('\n') {
        // Visible content = the line minus its terminator (`\n`, and a `\r` before it for CRLF).
        // `byte` still advances by the FULL `line_with_term.len()` below, so the terminator bytes
        // are accounted for exactly and the next line's class lookups stay aligned.
        let content = line_with_term.strip_suffix('\n').unwrap_or(line_with_term);
        let content = content.strip_suffix('\r').unwrap_or(content);
        let mut spans: Vec<Span> = Vec::new();
        // Group consecutive chars whose class (looked up at their absolute byte offset) is equal.
        // Iterating `char_indices` guarantees every cut lands on a char boundary.
        let mut run_start: Option<(usize, &'static str)> = None;
        for (local, _ch) in content.char_indices() {
            let class = class_per_byte
                .get(byte + local)
                .copied()
                .unwrap_or("tok-plain");
            match run_start {
                Some((start, run_class)) if run_class != class => {
                    spans.push(Span {
                        class: run_class,
                        text: content[start..local].to_string(),
                    });
                    run_start = Some((local, class));
                }
                None => run_start = Some((local, class)),
                _ => {}
            }
        }
        if let Some((start, run_class)) = run_start {
            spans.push(Span {
                class: run_class,
                text: content[start..].to_string(),
            });
        }
        out.push(spans);
        byte += line_with_term.len();
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

    #[test]
    fn segment_lines_crlf_and_multibyte_are_exact_and_boundary_safe() {
        // "#\r\n1é": a CRLF line then a line whose class boundary lands inside a 2-byte char.
        // Bytes: 0='#', 1='\r', 2='\n', 3='1', 4..5='é' (U+00E9 = 0xC3 0xA9). len == 6.
        // Under the old `lines()` + `+1` stride the running byte under-counted by 1 after the
        // CRLF line, shifting line 2's class boundary onto 'é''s continuation byte → mid-char
        // slice panic. This asserts (a) no panic and (b) 'é' stays intact.
        let text = "#\r\n1é";
        assert_eq!(text.len(), 6);
        let per_byte = [
            "tok-comment", // '#'
            "tok-plain",   // '\r'
            "tok-plain",   // '\n'
            "tok-number",  // '1'
            "tok-plain",   // 'é' byte 1
            "tok-plain",   // 'é' byte 2
        ];
        let out = segment_lines(text, &per_byte);
        assert_eq!(out.len(), 2);
        assert_eq!(
            out[0],
            vec![Span {
                class: "tok-comment",
                text: "#".into()
            }]
        );
        assert_eq!(
            out[1],
            vec![
                Span {
                    class: "tok-number",
                    text: "1".into()
                },
                Span {
                    class: "tok-plain",
                    text: "é".into()
                },
            ]
        );
    }

    #[test]
    fn segment_lines_pure_lf_multibyte_stays_correct() {
        // "café" on one pure-LF line: 'c','a','f' = keyword, 'é' (2 bytes) = string.
        // Bytes: 0='c',1='a',2='f',3..4='é'. The class boundary at the last char must not split é.
        let text = "café";
        assert_eq!(text.len(), 5);
        let per_byte = [
            "tok-keyword", // 'c'
            "tok-keyword", // 'a'
            "tok-keyword", // 'f'
            "tok-string",  // 'é' byte 1
            "tok-string",  // 'é' byte 2
        ];
        let out = segment_lines(text, &per_byte);
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0],
            vec![
                Span {
                    class: "tok-keyword",
                    text: "caf".into()
                },
                Span {
                    class: "tok-string",
                    text: "é".into()
                },
            ]
        );
    }
}
