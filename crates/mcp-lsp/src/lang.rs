//! The language dispatch table: file extension → (language server, LSP languageId), plus
//! binary resolution and a PATH executable probe. Pure logic (the PATH probe is the only
//! filesystem touch), so it is exhaustively unit-testable without spawning a server.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// One language server process. Several extensions may map to the same `ServerSpec` (e.g. all
/// of `.ts`/`.tsx`/`.js`/`.jsx` share one `typescript-language-server`), so the client registry
/// is keyed by `key`, not by extension or languageId.
pub struct ServerSpec {
    /// Registry key — dedups extensions that share one process.
    pub key: &'static str,
    /// Default executable name (overridable via `env_override`).
    pub default_bin: &'static str,
    /// Fixed argv passed after the binary (e.g. `--stdio`).
    pub args: &'static [&'static str],
    /// Env var whose value, if set, replaces `default_bin` (a bare executable path — no argv).
    pub env_override: &'static str,
    /// Timeout budget for the FIRST `lsp.diagnostics` call against this server, before its index
    /// is warm. Cold pyright/gopls indexing routinely exceeds the 15s steady-state default; a
    /// too-short budget returns `{diagnostics: [], timed_out: true}`, which reads as falsely
    /// "clean".
    pub first_open_diag_timeout: Duration,
}

pub static RUST_ANALYZER: ServerSpec = ServerSpec {
    key: "rust-analyzer",
    default_bin: "rust-analyzer",
    args: &[],
    env_override: "OTTO_RUST_ANALYZER_BIN",
    first_open_diag_timeout: Duration::from_secs(60),
};

pub static TYPESCRIPT: ServerSpec = ServerSpec {
    key: "typescript-language-server",
    default_bin: "typescript-language-server",
    args: &["--stdio"],
    env_override: "OTTO_TYPESCRIPT_LANGUAGE_SERVER_BIN",
    first_open_diag_timeout: Duration::from_secs(30),
};

pub static PYRIGHT: ServerSpec = ServerSpec {
    key: "pyright-langserver",
    default_bin: "pyright-langserver",
    args: &["--stdio"],
    env_override: "OTTO_PYRIGHT_LANGSERVER_BIN",
    first_open_diag_timeout: Duration::from_secs(60),
};

pub static GOPLS: ServerSpec = ServerSpec {
    key: "gopls",
    default_bin: "gopls",
    args: &[],
    env_override: "OTTO_GOPLS_BIN",
    first_open_diag_timeout: Duration::from_secs(60),
};

/// Every distinct server, for the startup availability gate.
pub static ALL_SERVERS: &[&ServerSpec] = &[&RUST_ANALYZER, &TYPESCRIPT, &PYRIGHT, &GOPLS];

/// Map a file extension (already lowercased, no leading dot) to its server + LSP languageId.
/// `None` ⇒ no language server configured for that extension.
pub fn config_for_extension(ext: &str) -> Option<(&'static ServerSpec, &'static str)> {
    match ext {
        "rs" => Some((&RUST_ANALYZER, "rust")),
        "ts" => Some((&TYPESCRIPT, "typescript")),
        "tsx" => Some((&TYPESCRIPT, "typescriptreact")),
        "js" | "mjs" | "cjs" => Some((&TYPESCRIPT, "javascript")),
        "jsx" => Some((&TYPESCRIPT, "javascriptreact")),
        "py" | "pyi" => Some((&PYRIGHT, "python")),
        "go" => Some((&GOPLS, "go")),
        _ => None,
    }
}

/// The executable for `spec`, given an optional override value (the `env_override`'s value).
/// Pure — the env read lives in `resolved_bin`, so this is directly testable.
pub fn resolved_bin_with(spec: &ServerSpec, override_val: Option<String>) -> String {
    override_val.unwrap_or_else(|| spec.default_bin.to_string())
}

/// The executable to spawn for `spec`: the `env_override` value if set, else `default_bin`.
pub fn resolved_bin(spec: &ServerSpec) -> String {
    resolved_bin_with(spec, std::env::var(spec.env_override).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extensions_map_to_the_right_server_and_language_id() {
        assert_eq!(config_for_extension("rs").unwrap().0.key, "rust-analyzer");
        assert_eq!(config_for_extension("rs").unwrap().1, "rust");
        assert_eq!(config_for_extension("go").unwrap().1, "go");
        assert_eq!(config_for_extension("py").unwrap().1, "python");
        assert_eq!(config_for_extension("pyi").unwrap().1, "python");
    }

    #[test]
    fn ts_js_family_shares_one_server_with_distinct_language_ids() {
        let key = "typescript-language-server";
        assert_eq!(config_for_extension("ts").unwrap().0.key, key);
        assert_eq!(config_for_extension("tsx").unwrap().0.key, key);
        assert_eq!(config_for_extension("js").unwrap().0.key, key);
        assert_eq!(config_for_extension("jsx").unwrap().0.key, key);
        assert_eq!(config_for_extension("ts").unwrap().1, "typescript");
        assert_eq!(config_for_extension("tsx").unwrap().1, "typescriptreact");
        assert_eq!(config_for_extension("js").unwrap().1, "javascript");
        assert_eq!(config_for_extension("mjs").unwrap().1, "javascript");
        assert_eq!(config_for_extension("cjs").unwrap().1, "javascript");
        assert_eq!(config_for_extension("jsx").unwrap().1, "javascriptreact");
    }

    #[test]
    fn unknown_and_empty_extensions_are_unsupported() {
        assert!(config_for_extension("txt").is_none());
        assert!(config_for_extension("").is_none());
        assert!(config_for_extension("md").is_none());
    }

    #[test]
    fn resolved_bin_defaults_without_an_override_and_honors_one() {
        assert_eq!(resolved_bin_with(&GOPLS, None), "gopls");
        assert_eq!(
            resolved_bin_with(&PYRIGHT, Some("/opt/custom-pyright".to_string())),
            "/opt/custom-pyright"
        );
    }
}
