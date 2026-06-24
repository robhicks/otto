//! Tree-sitter symbol chunking: parse a supported-language file into its named definitions
//! (functions, methods, types, classes) with their source spans. The ONLY tree-sitter-aware
//! module. `chunk_file` returns `None` for unsupported extensions or a file with no extractable
//! symbols — the caller then falls back to slice-1 whole-file indexing. Parsing is deterministic,
//! so chunks are a pure function of the file's content.

use std::path::Path;

use tree_sitter::{Language, Node, Parser};

/// The kind of a defined symbol. Coarse on purpose — used only for index metadata/debugging.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolKind {
    Function,
    Method,
    Type,
    Class,
}

impl SymbolKind {
    /// Stable lowercase tag stored in the `chunks.kind` column.
    pub fn as_str(&self) -> &'static str {
        match self {
            SymbolKind::Function => "function",
            SymbolKind::Method => "method",
            SymbolKind::Type => "type",
            SymbolKind::Class => "class",
        }
    }
}

/// One extracted symbol: its name, kind, 1-based line span, and source text (for body tokens).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolChunk {
    pub symbol: String,
    pub kind: SymbolKind,
    pub start_line: usize,
    pub end_line: usize,
    pub text: String,
}

/// Select a tree-sitter grammar by the path's file extension. `None` => unsupported language.
fn language_for(path: &Path) -> Option<Language> {
    let ext = path.extension()?.to_str()?.to_lowercase();
    let lang: Language = match ext.as_str() {
        "rs" => tree_sitter_rust::LANGUAGE.into(),
        "js" | "jsx" | "mjs" | "cjs" => tree_sitter_javascript::LANGUAGE.into(),
        "ts" => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        "tsx" => tree_sitter_typescript::LANGUAGE_TSX.into(),
        "py" => tree_sitter_python::LANGUAGE.into(),
        "go" => tree_sitter_go::LANGUAGE.into(),
        _ => return None,
    };
    Some(lang)
}

/// Map a tree-sitter node kind to a `SymbolKind`, across all supported grammars. `None` => not a
/// definition we capture. Node kinds are consistent enough across grammars to share one table.
fn symbol_kind(node_kind: &str) -> Option<SymbolKind> {
    Some(match node_kind {
        // functions (rust/js/go/python) + rust trait-method signatures
        "function_item"
        | "function_declaration"
        | "function_definition"
        | "function_signature_item" => SymbolKind::Function,
        // methods (js/ts/go)
        "method_definition" | "method_declaration" => SymbolKind::Method,
        // types (rust struct/enum/trait/type-alias; go type_spec; ts interface/type-alias)
        "struct_item"
        | "enum_item"
        | "trait_item"
        | "type_item"
        | "type_spec"
        | "type_declaration"
        | "type_alias_declaration"
        | "interface_declaration" => SymbolKind::Type,
        // classes (js/ts/python)
        "class_declaration" | "class_definition" => SymbolKind::Class,
        _ => return None,
    })
}

/// Recursively collect named definitions. Recursion is required so methods nested in `impl`/class
/// bodies are captured. A node with a captured kind but no `name` field (e.g. Go `type_declaration`
/// wraps a `type_spec` that carries the name) is skipped here and picked up via its child.
fn collect(node: Node, src: &[u8], out: &mut Vec<SymbolChunk>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if let Some(kind) = symbol_kind(child.kind()) {
            if let Some(name) = child.child_by_field_name("name") {
                if let Ok(symbol) = name.utf8_text(src) {
                    out.push(SymbolChunk {
                        symbol: symbol.to_string(),
                        kind,
                        start_line: child.start_position().row + 1,
                        end_line: child.end_position().row + 1,
                        text: child.utf8_text(src).unwrap_or_default().to_string(),
                    });
                }
            }
        }
        collect(child, src, out);
    }
}

/// Parse `content` (using the grammar chosen by `path`'s extension) into symbol chunks. `None` for
/// unsupported extensions or a file with no extractable symbols.
pub fn chunk_file(path: &Path, content: &str) -> Option<Vec<SymbolChunk>> {
    let lang = language_for(path)?;
    let mut parser = Parser::new();
    parser.set_language(&lang).ok()?;
    let tree = parser.parse(content, None)?;
    let mut out = Vec::new();
    collect(tree.root_node(), content.as_bytes(), &mut out);
    (!out.is_empty()).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn names(path: &str, src: &str) -> Vec<String> {
        chunk_file(&PathBuf::from(path), src)
            .unwrap_or_default()
            .into_iter()
            .map(|c| c.symbol)
            .collect()
    }

    #[test]
    fn rust_extracts_fn_struct_and_method() {
        let n = names(
            "src/auth.rs",
            "fn login() {}\nstruct Session {}\nimpl Session { fn renew(&self) {} }\n",
        );
        assert!(n.contains(&"login".to_string()), "{n:?}");
        assert!(n.contains(&"Session".to_string()), "{n:?}");
        assert!(n.contains(&"renew".to_string()), "{n:?}");
    }

    #[test]
    fn javascript_extracts_function_and_class() {
        let n = names(
            "app.js",
            "function loginUser() {}\nclass Auth { handle() {} }\n",
        );
        assert!(n.contains(&"loginUser".to_string()), "{n:?}");
        assert!(n.contains(&"Auth".to_string()), "{n:?}");
        assert!(n.contains(&"handle".to_string()), "{n:?}");
    }

    #[test]
    fn typescript_extracts_interface_and_function() {
        let n = names(
            "api.ts",
            "interface User { id: number }\nfunction getUser(): User { return null as any }\n",
        );
        assert!(n.contains(&"User".to_string()), "{n:?}");
        assert!(n.contains(&"getUser".to_string()), "{n:?}");
    }

    #[test]
    fn python_extracts_def_and_class() {
        let n = names(
            "svc.py",
            "def login():\n    pass\nclass Session:\n    def renew(self):\n        pass\n",
        );
        assert!(n.contains(&"login".to_string()), "{n:?}");
        assert!(n.contains(&"Session".to_string()), "{n:?}");
        assert!(n.contains(&"renew".to_string()), "{n:?}");
    }

    #[test]
    fn go_extracts_func_and_type() {
        let n = names(
            "svc.go",
            "package main\nfunc Login() {}\ntype Session struct {}\n",
        );
        assert!(n.contains(&"Login".to_string()), "{n:?}");
        assert!(n.contains(&"Session".to_string()), "{n:?}");
    }

    #[test]
    fn unsupported_extension_returns_none() {
        assert!(chunk_file(&PathBuf::from("notes.md"), "# login\nfn login").is_none());
        assert!(chunk_file(&PathBuf::from("Makefile"), "all:\n\tcargo build").is_none());
    }

    #[test]
    fn symbol_less_file_returns_none() {
        // A Rust file with only a const has no captured definitions -> whole-file fallback.
        assert!(chunk_file(&PathBuf::from("c.rs"), "const MAX: i32 = 5;\n").is_none());
    }

    #[test]
    fn span_lines_are_one_based() {
        let chunks = chunk_file(&PathBuf::from("a.rs"), "\nfn second_line() {}\n").unwrap();
        let c = chunks.iter().find(|c| c.symbol == "second_line").unwrap();
        assert_eq!(c.start_line, 2);
    }
}
