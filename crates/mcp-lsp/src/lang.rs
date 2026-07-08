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

/// Resolve `bin` to an executable file using a minimal PATH search. If `bin` contains a path
/// separator it is checked directly; otherwise each colon-separated entry of `path_var` is
/// tried. Returns the path only when the file exists AND has an executable bit set — a
/// present-but-non-executable file does not resolve. Unix-only, matching the OS sandbox's
/// Linux/macOS targeting (Windows PATHEXT/.cmd shims are out of scope).
pub fn resolve_executable(bin: &str, path_var: &str) -> Option<PathBuf> {
    if bin.contains('/') {
        let p = Path::new(bin);
        return is_executable(p).then(|| p.to_path_buf());
    }
    for dir in path_var.split(':').filter(|d| !d.is_empty()) {
        let candidate = Path::new(dir).join(bin);
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// True when at least one configured language server's resolved binary is on PATH. The startup
/// gate uses this: with no server present, `mcp-lsp` exits and the engine registers no `lsp.*`
/// tools (the additive-absence pattern).
pub fn any_server_available() -> bool {
    let path_var = std::env::var("PATH").unwrap_or_default();
    ALL_SERVERS
        .iter()
        .any(|spec| resolve_executable(&resolved_bin(spec), &path_var).is_some())
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

    use std::os::unix::fs::PermissionsExt;

    fn make_executable(path: &Path) {
        let mut perms = std::fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms).unwrap();
    }

    #[test]
    fn resolve_executable_finds_a_bare_name_on_path() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("myserver");
        std::fs::write(&bin, "#!/bin/sh\n").unwrap();
        make_executable(&bin);
        assert_eq!(
            resolve_executable("myserver", dir.path().to_str().unwrap()),
            Some(bin)
        );
    }

    #[test]
    fn resolve_executable_rejects_a_non_executable_file() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("myserver");
        std::fs::write(&bin, "not executable").unwrap();
        assert!(resolve_executable("myserver", dir.path().to_str().unwrap()).is_none());
    }

    #[test]
    fn resolve_executable_honors_a_path_separator_override() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("custom-ra");
        std::fs::write(&bin, "#!/bin/sh\n").unwrap();
        make_executable(&bin);
        assert_eq!(
            resolve_executable(bin.to_str().unwrap(), ""),
            Some(bin.clone())
        );
        let plain = dir.path().join("plain");
        std::fs::write(&plain, "x").unwrap();
        assert!(resolve_executable(plain.to_str().unwrap(), "").is_none());
    }

    #[test]
    fn resolve_executable_returns_none_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        assert!(resolve_executable("definitely-not-here", dir.path().to_str().unwrap()).is_none());
    }
}
