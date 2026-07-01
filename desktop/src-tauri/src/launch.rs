//! Pure, Tauri-free helpers for the sidecar bootstrap — unit-tested with plain `cargo test`,
//! no window/event loop/process needed. Keeps the logic that's actually worth testing out of
//! `lib.rs`'s Tauri-API-heavy glue code.

/// True when `line` (one line of the sidecar's stderr) is otto serve's own readiness message
/// (`crates/engine/src/main.rs`: `eprintln!("otto serve listening on {scheme}://{addr}/ws")`).
/// The port is fixed at 8787 by this slice's design (no `--port 0` support yet), so this is a
/// pure "has it started" signal — the address itself isn't parsed out of the line.
pub fn is_ready_line(line: &str) -> bool {
    line.contains("otto serve listening on")
}

/// Build the URL the desktop webview navigates to once the sidecar is ready: the existing
/// `ui/` app's index page with the auto-connect bootstrap query params. `ui/src/url.rs`'s
/// `parse_launch_params` is the reader side of this exact contract — the query key names
/// (`ws`, `token`, `autoconnect`) must match.
///
/// `ws_base` and `token` are never percent-encoded here: `ws_base` is always the fixed
/// `ws://127.0.0.1:8787` (no dynamic port this slice) and `token` is a `Uuid::new_v4()`
/// string (hex digits and hyphens only) — neither can contain a character that needs
/// percent-encoding, so encoding would be dead code.
pub fn build_launch_url(ws_base: &str, token: &str) -> String {
    format!("index.html?ws={ws_base}&token={token}&autoconnect=1")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ready_line_matches_otto_serves_own_readiness_message() {
        assert!(is_ready_line(
            "otto serve listening on ws://127.0.0.1:8787/ws"
        ));
        assert!(is_ready_line(
            "otto serve listening on wss://127.0.0.1:8787/ws"
        ));
    }

    #[test]
    fn ready_line_rejects_unrelated_output() {
        assert!(!is_ready_line(""));
        assert!(!is_ready_line("warning: something else"));
        assert!(!is_ready_line("otto run finished"));
    }

    #[test]
    fn launch_url_carries_ws_token_and_autoconnect() {
        assert_eq!(
            build_launch_url("ws://127.0.0.1:8787", "abc-123"),
            "index.html?ws=ws://127.0.0.1:8787&token=abc-123&autoconnect=1"
        );
    }
}
