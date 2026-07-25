//! Web syntax highlighting: a dependency-free, table-driven lexer.
//!
//! The desktop build highlights with native tree-sitter (`highlight_native`), which cannot be
//! reused here — tree-sitter's grammars are C built through `cc-rs`, and `wasm32-unknown-unknown`
//! ships no libc, so they fail to compile for the web target. The wasm-capable alternatives were
//! measured and rejected on bundle size (arborium +5.27 MB, syntect +1.50 MB, against a 2.54 MB
//! app). See `docs/superpowers/specs/2026-07-24-ui-dioxus-web-highlighting-design.md`.
//!
//! What makes a lexer sufficient: the shared token model has only five classes, and
//! `highlight_native` already collapses tree-sitter's full capture vocabulary down to those same
//! five. So the question per byte is not "what is this node" but "keyword, string, comment, type,
//! or number" — which a lexer answers directly. Both backends build the same `class_per_byte` map
//! and hand it to the same `tokens::segment_lines`, so web and desktop emit identical span classes
//! and share one `style.css`.
//!
//! This module is deliberately NOT `#[cfg]`-gated: it adds no dependency to gate, so compiling it
//! everywhere lets its tests run under the crate's ordinary `cargo test --features desktop`. Only
//! the `#[cfg(feature = "web")]` arm in `editor/mod.rs` calls it.
//!
//! Accuracy is heuristic where tree-sitter's is structural — most visibly, `tok-type` is assigned
//! by convention (known builtin type names, plus an uppercase initial) rather than by resolving a
//! type position. Errors are bounded to a single mis-coloured token; the scanners below all
//! terminate, so no input can produce a runaway highlight state.

use crate::editor::tokens::{plain_spans, segment_lines, Span};

/// The class vocabulary, shared with `highlight_native` and `style.css`. Anything not classified
/// stays `PLAIN`.
const PLAIN: &str = "tok-plain";
const KEYWORD: &str = "tok-keyword";
const STRING: &str = "tok-string";
const COMMENT: &str = "tok-comment";
const TYPE: &str = "tok-type";
const NUMBER: &str = "tok-number";

/// The lexical rules for one language. Everything the scanner branches on lives here, so adding a
/// language is a table entry rather than new control flow.
struct LangSpec {
    /// Prefixes that start a comment running to end of line.
    line_comments: &'static [&'static str],
    /// `(open, close)` for block comments, if the language has them.
    block_comment: Option<(&'static str, &'static str)>,
    /// Whether block comments nest (Rust does; C-family ones do not).
    nested_block_comments: bool,
    /// Quote characters that open a single-line string. Backtick is handled separately since it
    /// spans lines.
    quotes: &'static [u8],
    /// Backtick opens a multi-line string (JS template literals, Go raw strings).
    backtick_strings: bool,
    /// Python-style `"""` / `'''` strings, which span lines. Checked before `quotes`.
    triple_quotes: bool,
    /// Rust-style raw strings: `r"..."`, `r#"..."#`, `r##"..."##`.
    raw_strings: bool,
    /// The language has `'a` lifetimes, so a `'` is only a literal when it closes like one.
    /// Without this, a lifetime would open a string and mis-colour the rest of the line.
    lifetimes: bool,
    keywords: &'static [&'static str],
    /// Builtin type names. Other types are picked up by the uppercase-initial heuristic.
    types: &'static [&'static str],
}

const RUST: LangSpec = LangSpec {
    line_comments: &["//"],
    block_comment: Some(("/*", "*/")),
    nested_block_comments: true,
    quotes: b"\"",
    backtick_strings: false,
    triple_quotes: false,
    raw_strings: true,
    lifetimes: true,
    keywords: &[
        "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum",
        "extern", "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move",
        "mut", "pub", "ref", "return", "self", "Self", "static", "struct", "super", "trait",
        "true", "type", "union", "unsafe", "use", "where", "while",
    ],
    types: &[
        "bool", "char", "f32", "f64", "i8", "i16", "i32", "i64", "i128", "isize", "str", "u8",
        "u16", "u32", "u64", "u128", "usize",
    ],
};

/// Shared by `javascript` and `typescript`; TypeScript layers its own keywords/types on top.
const JS_KEYWORDS: &[&str] = &[
    "async",
    "await",
    "break",
    "case",
    "catch",
    "class",
    "const",
    "continue",
    "debugger",
    "default",
    "delete",
    "do",
    "else",
    "export",
    "extends",
    "false",
    "finally",
    "for",
    "function",
    "if",
    "import",
    "in",
    "instanceof",
    "let",
    "new",
    "null",
    "of",
    "return",
    "static",
    "super",
    "switch",
    "this",
    "throw",
    "true",
    "try",
    "typeof",
    "undefined",
    "var",
    "void",
    "while",
    "with",
    "yield",
];

const JAVASCRIPT: LangSpec = LangSpec {
    line_comments: &["//"],
    block_comment: Some(("/*", "*/")),
    nested_block_comments: false,
    quotes: b"\"'",
    backtick_strings: true,
    triple_quotes: false,
    raw_strings: false,
    lifetimes: false,
    keywords: JS_KEYWORDS,
    types: &[],
};

const TYPESCRIPT: LangSpec = LangSpec {
    line_comments: &["//"],
    block_comment: Some(("/*", "*/")),
    nested_block_comments: false,
    quotes: b"\"'",
    backtick_strings: true,
    triple_quotes: false,
    raw_strings: false,
    lifetimes: false,
    keywords: &[
        "abstract",
        "as",
        "async",
        "await",
        "break",
        "case",
        "catch",
        "class",
        "const",
        "continue",
        "debugger",
        "declare",
        "default",
        "delete",
        "do",
        "else",
        "enum",
        "export",
        "extends",
        "false",
        "finally",
        "for",
        "function",
        "if",
        "implements",
        "import",
        "in",
        "instanceof",
        "interface",
        "is",
        "keyof",
        "let",
        "namespace",
        "new",
        "null",
        "of",
        "private",
        "protected",
        "public",
        "readonly",
        "return",
        "satisfies",
        "static",
        "super",
        "switch",
        "this",
        "throw",
        "true",
        "try",
        "type",
        "typeof",
        "undefined",
        "var",
        "void",
        "while",
        "yield",
    ],
    types: &[
        "any", "bigint", "boolean", "never", "number", "object", "string", "symbol", "unknown",
    ],
};

const PYTHON: LangSpec = LangSpec {
    line_comments: &["#"],
    block_comment: None,
    nested_block_comments: false,
    quotes: b"\"'",
    backtick_strings: false,
    triple_quotes: true,
    raw_strings: false,
    lifetimes: false,
    keywords: &[
        "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del",
        "elif", "else", "except", "False", "finally", "for", "from", "global", "if", "import",
        "in", "is", "lambda", "None", "nonlocal", "not", "or", "pass", "raise", "return", "True",
        "try", "while", "with", "yield",
    ],
    types: &[
        "bool",
        "bytes",
        "complex",
        "dict",
        "float",
        "frozenset",
        "int",
        "list",
        "object",
        "set",
        "str",
        "tuple",
    ],
};

const GO: LangSpec = LangSpec {
    line_comments: &["//"],
    block_comment: Some(("/*", "*/")),
    nested_block_comments: false,
    quotes: b"\"'",
    backtick_strings: true,
    triple_quotes: false,
    raw_strings: false,
    lifetimes: false,
    keywords: &[
        "break",
        "case",
        "chan",
        "const",
        "continue",
        "default",
        "defer",
        "else",
        "fallthrough",
        "false",
        "for",
        "func",
        "go",
        "goto",
        "if",
        "import",
        "interface",
        "map",
        "nil",
        "package",
        "range",
        "return",
        "select",
        "struct",
        "switch",
        "true",
        "type",
        "var",
    ],
    types: &[
        "any",
        "bool",
        "byte",
        "complex64",
        "complex128",
        "error",
        "float32",
        "float64",
        "int",
        "int8",
        "int16",
        "int32",
        "int64",
        "rune",
        "string",
        "uint",
        "uint8",
        "uint16",
        "uint32",
        "uint64",
        "uintptr",
    ],
};

/// Map a `language_for_path` id to its spec. `None` => unsupported, caller falls back to
/// `plain_spans`. Mirrors the language set `highlight_native` covers.
fn spec(lang: &str) -> Option<&'static LangSpec> {
    Some(match lang {
        "rust" => &RUST,
        "javascript" => &JAVASCRIPT,
        "typescript" => &TYPESCRIPT,
        "python" => &PYTHON,
        "go" => &GO,
        _ => return None,
    })
}

/// Highlight `text` for `lang`, falling back to `plain_spans` for unsupported languages — the same
/// best-effort contract `highlight_native::highlight` has.
pub fn highlight(text: &str, lang: &str) -> Vec<Vec<Span>> {
    match spec(lang) {
        Some(spec) => segment_lines(text, &class_map(text, spec)),
        None => plain_spans(text),
    }
}

/// True for bytes that may appear inside an identifier. Every byte of a multibyte UTF-8 char is
/// `>= 0x80` and so counts as an identifier byte — which keeps a non-ASCII identifier (or a stray
/// non-ASCII char) scanned as one whole run, so a class boundary can never land mid-char.
fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80
}

/// Build the per-source-byte class map `segment_lines` consumes. Every branch advances `i` by at
/// least one byte, so this always terminates.
fn class_map(text: &str, spec: &LangSpec) -> Vec<&'static str> {
    let b = text.as_bytes();
    let mut out = vec![PLAIN; b.len()];
    let mut i = 0usize;
    while i < b.len() {
        // Order matters: the longer/more specific openers must be tried before the shorter ones
        // they'd otherwise be swallowed by (triple quotes before quotes, raw-string `r"` before
        // the identifier scan, block comments before the `/` falls through as punctuation).
        if let Some(end) = scan_line_comment(b, i, spec) {
            paint(&mut out, i, end, COMMENT);
            i = end;
        } else if let Some(end) = scan_block_comment(b, i, spec) {
            paint(&mut out, i, end, COMMENT);
            i = end;
        } else if let Some(end) = scan_raw_string(b, i, spec) {
            paint(&mut out, i, end, STRING);
            i = end;
        } else if let Some(end) = scan_triple_quoted(b, i, spec) {
            paint(&mut out, i, end, STRING);
            i = end;
        } else if let Some(end) = scan_rust_char_literal(text, b, i, spec) {
            paint(&mut out, i, end, STRING);
            i = end;
        } else if spec.lifetimes && b[i] == b'\'' {
            // A `'` in a lifetime-bearing language that did not close like a char literal is a
            // lifetime tick. Leave it plain and step over it so the name after it scans as an
            // ordinary identifier — the alternative (treating it as a string opener) would run the
            // string class to end of line.
            i += 1;
        } else if let Some(end) = scan_string(b, i, spec) {
            paint(&mut out, i, end, STRING);
            i = end;
        } else if b[i].is_ascii_digit() {
            let end = scan_number(b, i);
            paint(&mut out, i, end, NUMBER);
            i = end;
        } else if is_ident_byte(b[i]) {
            let end = scan_ident(b, i);
            paint(&mut out, i, end, classify_ident(&text[i..end], spec));
            i = end;
        } else {
            i += 1;
        }
    }
    out
}

fn paint(out: &mut [&'static str], start: usize, end: usize, class: &'static str) {
    let end = end.min(out.len());
    for slot in &mut out[start..end] {
        *slot = class;
    }
}

fn starts_with(b: &[u8], i: usize, pat: &str) -> bool {
    b.len() >= i + pat.len() && &b[i..i + pat.len()] == pat.as_bytes()
}

/// End of line from `i`, EXCLUDING the `\n` itself so line terminators stay `tok-plain` — which is
/// what `segment_lines` expects (it strips terminators and never reads their class).
fn line_end(b: &[u8], i: usize) -> usize {
    match b[i..].iter().position(|&c| c == b'\n') {
        Some(off) => i + off,
        None => b.len(),
    }
}

fn scan_line_comment(b: &[u8], i: usize, spec: &LangSpec) -> Option<usize> {
    spec.line_comments
        .iter()
        .find(|p| starts_with(b, i, p))
        .map(|_| line_end(b, i))
}

/// Block comment from its opener to the matching close, or to EOF if unterminated. Nesting is
/// tracked when the language nests (Rust), so `/* /* */ */` closes once, not twice.
fn scan_block_comment(b: &[u8], i: usize, spec: &LangSpec) -> Option<usize> {
    let (open, close) = spec.block_comment?;
    if !starts_with(b, i, open) {
        return None;
    }
    let mut depth = 1usize;
    let mut j = i + open.len();
    while j < b.len() {
        if starts_with(b, j, close) {
            depth -= 1;
            j += close.len();
            if depth == 0 {
                return Some(j);
            }
        } else if spec.nested_block_comments && starts_with(b, j, open) {
            depth += 1;
            j += open.len();
        } else {
            j += 1;
        }
    }
    Some(b.len())
}

/// Rust raw strings: `r"…"`, `r#"…"#`, `r##"…"##`. The hash count must match to close, which is
/// the whole point of the form (it lets the body contain quotes).
fn scan_raw_string(b: &[u8], i: usize, spec: &LangSpec) -> Option<usize> {
    if !spec.raw_strings || b[i] != b'r' {
        return None;
    }
    // `foo r"x"` is a raw string but `bar"x"` is not — only a `r` that begins a token counts.
    if i > 0 && is_ident_byte(b[i - 1]) {
        return None;
    }
    let mut j = i + 1;
    let hash_start = j;
    while j < b.len() && b[j] == b'#' {
        j += 1;
    }
    let hashes = j - hash_start;
    if j >= b.len() || b[j] != b'"' {
        return None;
    }
    j += 1;
    while j < b.len() {
        if b[j] == b'"'
            && b[j + 1..]
                .iter()
                .take(hashes)
                .filter(|&&c| c == b'#')
                .count()
                == hashes
        {
            return Some(j + 1 + hashes);
        }
        j += 1;
    }
    Some(b.len())
}

/// Python `"""…"""` / `'''…'''`, which span lines.
fn scan_triple_quoted(b: &[u8], i: usize, spec: &LangSpec) -> Option<usize> {
    if !spec.triple_quotes {
        return None;
    }
    let delim = [r#"""""#, "'''"]
        .into_iter()
        .find(|d| starts_with(b, i, d))?;
    let mut j = i + delim.len();
    while j < b.len() {
        if starts_with(b, j, delim) {
            return Some(j + delim.len());
        }
        if b[j] == b'\\' {
            j += 2;
            continue;
        }
        j += 1;
    }
    Some(b.len())
}

/// A Rust `'` that really is a char literal (`'a'`, `'\n'`, `'é'`) rather than a lifetime tick.
/// Returns `None` for a lifetime so the caller can step over the tick instead.
fn scan_rust_char_literal(text: &str, b: &[u8], i: usize, spec: &LangSpec) -> Option<usize> {
    if !spec.lifetimes || b[i] != b'\'' {
        return None;
    }
    if b.get(i + 1) == Some(&b'\\') {
        // Escaped: `'\n'`, `'\''`, `'\u{1F600}'`. Scan to the closing tick, bounded so a stray
        // backslash-quote cannot run away.
        let mut j = i + 2;
        while j < b.len() && j < i + 16 {
            if b[j] == b'\'' {
                return Some(j + 1);
            }
            if b[j] == b'\n' {
                return None;
            }
            j += 1;
        }
        return None;
    }
    // Unescaped: exactly one char between the ticks. Decoding the char (rather than assuming one
    // byte) is what makes `'é'` a literal and keeps the returned end on a char boundary.
    let inner = text.get(i + 1..)?.chars().next()?;
    let close = i + 1 + inner.len_utf8();
    (b.get(close) == Some(&b'\'')).then_some(close + 1)
}

/// A single-line quoted string, or a backtick string where the language has them (JS template
/// literals, Go raw strings). Single-line strings stop at the newline so an unterminated quote
/// mis-colours one line rather than the rest of the file.
fn scan_string(b: &[u8], i: usize, spec: &LangSpec) -> Option<usize> {
    let quote = b[i];
    let multiline = quote == b'`' && spec.backtick_strings;
    if !multiline && !spec.quotes.contains(&quote) {
        return None;
    }
    // Go's backtick string is raw: a backslash in it is a literal backslash, not an escape.
    let escapes = quote != b'`';
    let mut j = i + 1;
    while j < b.len() {
        if escapes && b[j] == b'\\' {
            j += 2;
            continue;
        }
        if b[j] == quote {
            return Some(j + 1);
        }
        if b[j] == b'\n' && !multiline {
            return Some(j);
        }
        j += 1;
    }
    Some(b.len())
}

/// A numeric literal: digits plus the letters/underscores that belong to a radix prefix or suffix
/// (`0xFF`, `1_000`, `10u32`), an interior `.` only when a digit follows (so Rust's `1..2` range
/// and a `1.foo()` method call are not swallowed), and a signed exponent.
fn scan_number(b: &[u8], i: usize) -> usize {
    let mut j = i;
    while j < b.len() {
        let c = b[j];
        let digit_follows = b.get(j + 1).is_some_and(u8::is_ascii_digit);
        let continues_literal = c.is_ascii_alphanumeric()
            || c == b'_'
            // `.` continues the literal only when a digit follows it.
            || (c == b'.' && digit_follows)
            // A sign belongs to the literal only as an exponent's, i.e. directly after `e`/`E`
            // and followed by a digit. `j > i` holds implicitly: position `i` is a digit, so the
            // first iteration always takes the alphanumeric branch and `b[j - 1]` is in bounds.
            || ((c == b'+' || c == b'-') && matches!(b[j - 1], b'e' | b'E') && digit_follows);
        if !continues_literal {
            break;
        }
        j += 1;
    }
    j
}

fn scan_ident(b: &[u8], i: usize) -> usize {
    let mut j = i;
    while j < b.len() && is_ident_byte(b[j]) {
        j += 1;
    }
    j
}

/// Keyword beats builtin type beats the uppercase-initial convention. The last is the heuristic
/// the design note calls out: it colours `Vec`/`MyStruct` as types, and also colours SCREAMING
/// constants as types, which is the accepted inaccuracy of a lexer-based highlighter.
fn classify_ident(word: &str, spec: &LangSpec) -> &'static str {
    if spec.keywords.contains(&word) {
        KEYWORD
    } else if spec.types.contains(&word) || word.chars().next().is_some_and(char::is_uppercase) {
        TYPE
    } else {
        PLAIN
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Classes present on the spans covering `needle`'s first occurrence in `src`.
    fn class_of(src: &str, lang: &str, needle: &str) -> Vec<&'static str> {
        let spec = spec(lang).expect("supported lang");
        let map = class_map(src, spec);
        let at = src.find(needle).expect("needle present in source");
        map[at..at + needle.len()].to_vec()
    }

    /// Assert every byte of `needle` carries exactly `class`.
    fn assert_class(src: &str, lang: &str, needle: &str, class: &str) {
        let got = class_of(src, lang, needle);
        assert!(
            got.iter().all(|c| *c == class),
            "{lang}: expected {needle:?} to be all {class}, got {got:?}"
        );
    }

    #[test]
    fn unsupported_lang_falls_back_to_plain() {
        // Same contract as highlight_native: never break the editor on an unknown language.
        let src = "key = value\nother";
        assert_eq!(highlight(src, "toml"), plain_spans(src));
        assert_eq!(highlight(src, "text"), plain_spans(src));
    }

    #[test]
    fn class_map_length_always_matches_source_bytes() {
        // segment_lines indexes this map by absolute byte offset, so a short map would silently
        // read `tok-plain` past the end.
        for lang in ["rust", "javascript", "typescript", "python", "go"] {
            let src = "a \"é\" /* ünïcöde */ 42\n\tx";
            let spec = spec(lang).unwrap();
            assert_eq!(class_map(src, spec).len(), src.len(), "{lang}");
        }
    }

    #[test]
    fn rust_classifies_all_five_classes() {
        let src = "// note\nfn f() -> u32 { let s = \"hi\"; 42 }";
        assert_class(src, "rust", "// note", COMMENT);
        assert_class(src, "rust", "fn", KEYWORD);
        assert_class(src, "rust", "u32", TYPE);
        assert_class(src, "rust", "\"hi\"", STRING);
        assert_class(src, "rust", "42", NUMBER);
    }

    #[test]
    fn javascript_classifies_all_five_classes() {
        let src = "// note\nconst x = 'hi'; /* b */ let n = 3.5; new Foo();";
        assert_class(src, "javascript", "// note", COMMENT);
        assert_class(src, "javascript", "const", KEYWORD);
        assert_class(src, "javascript", "'hi'", STRING);
        assert_class(src, "javascript", "3.5", NUMBER);
        assert_class(src, "javascript", "Foo", TYPE);
    }

    #[test]
    fn typescript_classifies_builtin_types_and_keywords() {
        let src = "interface A { n: number; s: string }\nconst t: boolean = true;";
        assert_class(src, "typescript", "interface", KEYWORD);
        assert_class(src, "typescript", "number", TYPE);
        assert_class(src, "typescript", "string", TYPE);
        assert_class(src, "typescript", "boolean", TYPE);
    }

    #[test]
    fn python_classifies_all_five_classes() {
        let src = "# note\ndef f(x: int) -> str:\n    return 'hi' + str(7)";
        assert_class(src, "python", "# note", COMMENT);
        assert_class(src, "python", "def", KEYWORD);
        assert_class(src, "python", "int", TYPE);
        assert_class(src, "python", "'hi'", STRING);
        assert_class(src, "python", "7", NUMBER);
    }

    #[test]
    fn go_classifies_all_five_classes() {
        let src = "// note\nfunc f(n int) string { return \"hi\" + \"x\" }\nconst k = 9";
        assert_class(src, "go", "// note", COMMENT);
        assert_class(src, "go", "func", KEYWORD);
        assert_class(src, "go", "int", TYPE);
        assert_class(src, "go", "\"hi\"", STRING);
        assert_class(src, "go", "9", NUMBER);
    }

    #[test]
    fn rust_lifetime_is_not_a_string() {
        // The regression this guards: treating `'` as a string opener makes a lifetime swallow the
        // rest of the line, so `&'a str` would colour `a str) -> u32 {` as string.
        let src = "fn f<'a>(s: &'a str) -> u32 { 1 }";
        assert_class(src, "rust", "u32", TYPE);
        assert_class(src, "rust", "1", NUMBER);
        let map = class_map(src, spec("rust").unwrap());
        let tick = src.find("&'a").unwrap() + 1;
        assert_eq!(map[tick], PLAIN, "lifetime tick should stay plain");
        assert_eq!(map[tick + 1], PLAIN, "lifetime name should stay plain");
    }

    #[test]
    fn rust_char_literals_are_strings_including_escapes_and_multibyte() {
        assert_class("let c = 'x';", "rust", "'x'", STRING);
        assert_class("let c = '\\n';", "rust", "'\\n'", STRING);
        assert_class("let c = 'é';", "rust", "'é'", STRING);
    }

    #[test]
    fn rust_raw_strings_close_on_matching_hashes() {
        let src = "let s = r#\"a \"quoted\" b\"#; let n = 5;";
        assert_class(src, "rust", "r#\"a \"quoted\" b\"#", STRING);
        // The literal must actually close, or `5` would be swallowed into the string.
        assert_class(src, "rust", "5", NUMBER);
    }

    #[test]
    fn rust_block_comments_nest() {
        // Non-nesting scanners close at the inner `*/`, leaving `still */` unhighlighted and `7`
        // wrongly outside the comment.
        let src = "/* a /* b */ still */ let n = 7;";
        assert_class(src, "rust", "/* a /* b */ still */", COMMENT);
        assert_class(src, "rust", "let", KEYWORD);
        assert_class(src, "rust", "7", NUMBER);
    }

    #[test]
    fn c_family_block_comments_do_not_nest() {
        let src = "/* a /* b */ x = 1;";
        assert_class(src, "javascript", "/* a /* b */", COMMENT);
        assert_class(src, "javascript", "1", NUMBER);
    }

    #[test]
    fn python_triple_quoted_strings_span_lines() {
        let src = "x = \"\"\"doc\nmore # not a comment\n\"\"\"\ny = 2";
        assert_class(
            src,
            "python",
            "\"\"\"doc\nmore # not a comment\n\"\"\"",
            STRING,
        );
        assert_class(src, "python", "2", NUMBER);
    }

    #[test]
    fn backtick_strings_span_lines_in_js_and_go() {
        let js = "const t = `a\nb`; const n = 4;";
        assert_class(js, "javascript", "`a\nb`", STRING);
        assert_class(js, "javascript", "4", NUMBER);
        let go = "s := `raw \\n not escaped`\nn := 4";
        assert_class(go, "go", "`raw \\n not escaped`", STRING);
        assert_class(go, "go", "4", NUMBER);
    }

    #[test]
    fn unterminated_string_stops_at_end_of_line() {
        // Containment: a typo'd quote must not colour the whole rest of the file.
        let src = "let s = \"oops\nfn g() {}";
        assert_class(src, "rust", "fn", KEYWORD);
        let map = class_map(src, spec("rust").unwrap());
        assert_eq!(map[src.find('\n').unwrap()], PLAIN);
    }

    #[test]
    fn escaped_quote_does_not_close_the_string() {
        let src = "let s = \"a\\\"b\"; let n = 6;";
        assert_class(src, "rust", "\"a\\\"b\"", STRING);
        assert_class(src, "rust", "6", NUMBER);
    }

    #[test]
    fn numbers_do_not_swallow_ranges_or_method_calls() {
        // `.` only continues a number when a digit follows it.
        assert_class("for i in 1..9 {}", "rust", "1", NUMBER);
        assert_class("for i in 1..9 {}", "rust", "9", NUMBER);
        let src = "let t = 1.max(2);";
        let map = class_map(src, spec("rust").unwrap());
        assert_eq!(
            map[src.find("max").unwrap()],
            PLAIN,
            "`max` is not a number"
        );
    }

    #[test]
    fn numeric_literal_forms_are_fully_covered() {
        assert_class("let a = 0xFF;", "rust", "0xFF", NUMBER);
        assert_class("let a = 1_000;", "rust", "1_000", NUMBER);
        assert_class("let a = 10u32;", "rust", "10u32", NUMBER);
        assert_class("let a = 1.5e-3;", "rust", "1.5e-3", NUMBER);
    }

    #[test]
    fn digits_inside_an_identifier_are_not_a_number() {
        let src = "let foo2 = 1;";
        let map = class_map(src, spec("rust").unwrap());
        let at = src.find("foo2").unwrap();
        assert!(
            map[at..at + 4].iter().all(|c| *c == PLAIN),
            "identifier `foo2` should be one plain run, got {:?}",
            &map[at..at + 4]
        );
    }

    #[test]
    fn multibyte_content_never_splits_a_char() {
        // The failure mode: painting a class boundary onto a UTF-8 continuation byte, which makes
        // segment_lines slice mid-char and panic. Driving it through the real seam proves it.
        for lang in ["rust", "javascript", "typescript", "python", "go"] {
            let src = "x = \"héllo wörld\"\ny = ünïcödé\nz = 1";
            let out = highlight(src, lang);
            let rebuilt: String = out
                .iter()
                .map(|line| line.iter().map(|s| s.text.as_str()).collect::<String>())
                .collect::<Vec<_>>()
                .join("\n");
            assert_eq!(rebuilt, src, "{lang}: spans must reproduce the source");
        }
    }

    #[test]
    fn spans_reproduce_the_source_exactly_including_crlf() {
        // segment_lines drops line terminators, so joining with \n must round-trip CRLF input to
        // its LF form — the same normalization the desktop path produces.
        let src = "// c\r\nfn f() {}\r\nlet n = 1;";
        let rebuilt: String = highlight(src, "rust")
            .iter()
            .map(|line| line.iter().map(|s| s.text.as_str()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(rebuilt, src.replace("\r\n", "\n"));
    }

    #[test]
    fn emits_only_the_shared_class_vocabulary() {
        // The contract that lets one style.css serve both targets: web may never invent a class
        // desktop does not also emit.
        const VOCAB: [&str; 6] = [PLAIN, KEYWORD, STRING, COMMENT, TYPE, NUMBER];
        for lang in ["rust", "javascript", "typescript", "python", "go"] {
            let src = "// c\nfn f(x: int) { let s = \"a\"; return 1; } # h\nclass Foo: pass";
            for span in highlight(src, lang).iter().flatten() {
                assert!(VOCAB.contains(&span.class), "{lang}: stray class {span:?}");
            }
        }
    }

    #[test]
    fn empty_and_whitespace_input_are_handled() {
        assert!(highlight("", "rust").is_empty());
        assert_eq!(highlight("\n\n", "rust").len(), 2);
        assert_eq!(highlight("   ", "rust")[0][0].class, PLAIN);
    }
}
