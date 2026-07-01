//! Pure, browser-free helpers. Unit-tested with plain `cargo test` on the host
//! (no wasm, no DOM) — this is the determinism seam for the UI's logic.

/// Build the `/ws` connection URL. `session`/`last_seq` are appended only when reconnecting.
pub fn build_ws_url(
    base: &str,
    token: &str,
    session: Option<&str>,
    last_seq: Option<u64>,
) -> String {
    // Normalize the base: tolerate a trailing slash and a base that already ends in
    // `/ws` (e.g. the endpoint URL `otto serve` prints), so we never produce `/ws/ws`.
    let base = base.trim_end_matches('/');
    let base = base.strip_suffix("/ws").unwrap_or(base);
    let mut url = format!("{base}/ws?token={}", urlencoding::encode(token));
    if let Some(s) = session {
        url.push_str(&format!("&session={}", urlencoding::encode(s)));
    }
    if let Some(seq) = last_seq {
        url.push_str(&format!("&last_seq={seq}"));
    }
    url
}

/// True if an incoming event seq is newer than what we've applied
/// (never re-apply an event with seq <= the current high-water mark).
pub fn should_apply(current: Option<u64>, incoming: u64) -> bool {
    match current {
        Some(c) => incoming > c,
        None => true,
    }
}

/// Advance the high-water seq mark; never moves backwards.
pub fn advance_last_seq(current: Option<u64>, incoming: u64) -> Option<u64> {
    match current {
        Some(c) if incoming <= c => Some(c),
        _ => Some(incoming),
    }
}

/// Derive the HTTP origin for the `/workspace` RPC from the `ws`/`wss` connection URL.
/// `ws://…`→`http://…`, `wss://…`→`https://…`; a trailing slash and a `/ws` suffix are
/// trimmed (the UI form may hold the endpoint URL `otto serve` prints).
pub fn ws_to_http_base(ws_url: &str) -> String {
    let trimmed = ws_url.trim_end_matches('/');
    let trimmed = trimmed.strip_suffix("/ws").unwrap_or(trimmed);
    if let Some(rest) = trimmed.strip_prefix("wss://") {
        format!("https://{rest}")
    } else if let Some(rest) = trimmed.strip_prefix("ws://") {
        format!("http://{rest}")
    } else {
        trimmed.to_string()
    }
}

/// The Tauri desktop wrapper's auto-connect bootstrap (sub-project G): the local sidecar's
/// WS base URL and a freshly-generated bearer token, carried as query params on the webview's
/// initial navigation (`desktop/src-tauri/src/launch.rs`'s `build_launch_url` is the writer
/// side of this contract).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchParams {
    pub ws: String,
    pub token: String,
}

/// Parse `ws`/`token`/`autoconnect` from a query string (with or without a leading `?`, as
/// returned by `web_sys`'s `Location::search()`). Returns `Some` only when `autoconnect=1` and
/// both `ws` and `token` are present and non-empty — anything else (a plain browser visit with
/// no query string, a manually-typed URL, a malformed/partial query) yields `None`, leaving the
/// existing manual connect form as the fallback. Unknown keys and malformed `key=value` pairs
/// are silently ignored, not an error.
pub fn parse_launch_params(query: &str) -> Option<LaunchParams> {
    let query = query.strip_prefix('?').unwrap_or(query);
    let mut ws = None;
    let mut token = None;
    let mut autoconnect = false;
    for pair in query.split('&') {
        let Some((key, raw_value)) = pair.split_once('=') else {
            continue;
        };
        let Ok(value) = urlencoding::decode(raw_value) else {
            continue;
        };
        match key {
            "ws" => ws = Some(value.into_owned()),
            "token" => token = Some(value.into_owned()),
            "autoconnect" => autoconnect = value == "1",
            _ => {}
        }
    }
    if !autoconnect {
        return None;
    }
    let ws = ws.filter(|s| !s.is_empty())?;
    let token = token.filter(|s| !s.is_empty())?;
    Some(LaunchParams { ws, token })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_without_reconnect_has_only_token() {
        let u = build_ws_url("ws://127.0.0.1:8787", "tok", None, None);
        assert_eq!(u, "ws://127.0.0.1:8787/ws?token=tok");
    }

    #[test]
    fn url_trims_trailing_slash_and_encodes_token() {
        let u = build_ws_url("ws://host/", "a b&c", None, None);
        assert_eq!(u, "ws://host/ws?token=a%20b%26c");
    }

    #[test]
    fn url_with_reconnect_appends_session_and_seq() {
        let u = build_ws_url("ws://h", "t", Some("sess-1"), Some(12));
        assert_eq!(u, "ws://h/ws?token=t&session=sess-1&last_seq=12");
    }

    #[test]
    fn should_apply_only_for_strictly_newer() {
        assert!(should_apply(None, 0));
        assert!(should_apply(Some(5), 6));
        assert!(!should_apply(Some(5), 5));
        assert!(!should_apply(Some(5), 4));
    }

    #[test]
    fn advance_never_goes_backwards() {
        assert_eq!(advance_last_seq(None, 0), Some(0));
        assert_eq!(advance_last_seq(Some(3), 7), Some(7));
        assert_eq!(advance_last_seq(Some(7), 3), Some(7));
    }

    #[test]
    fn ws_to_http_base_maps_schemes_and_strips_ws_suffix() {
        assert_eq!(
            ws_to_http_base("ws://127.0.0.1:8787"),
            "http://127.0.0.1:8787"
        );
        assert_eq!(ws_to_http_base("wss://host:9000"), "https://host:9000");
        assert_eq!(ws_to_http_base("ws://h/ws"), "http://h");
        assert_eq!(ws_to_http_base("ws://h/ws/"), "http://h");
        // A non-ws base is passed through untouched (only trailing slash/`/ws` trimmed).
        assert_eq!(ws_to_http_base("http://h:1/"), "http://h:1");
    }

    #[test]
    fn url_tolerates_base_already_ending_in_ws() {
        assert_eq!(
            build_ws_url("ws://127.0.0.1:8787/ws", "tok", None, None),
            "ws://127.0.0.1:8787/ws?token=tok"
        );
        // trailing slash + /ws both handled
        assert_eq!(
            build_ws_url("ws://h/ws/", "t", None, None),
            "ws://h/ws?token=t"
        );
    }

    #[test]
    fn launch_params_requires_ws_token_and_autoconnect() {
        assert_eq!(
            parse_launch_params("ws=ws%3A%2F%2F127.0.0.1%3A8787&token=abc-123&autoconnect=1"),
            Some(LaunchParams {
                ws: "ws://127.0.0.1:8787".to_string(),
                token: "abc-123".to_string(),
            })
        );
    }

    #[test]
    fn launch_params_tolerates_leading_question_mark() {
        assert_eq!(
            parse_launch_params("?ws=ws://h&token=t&autoconnect=1"),
            Some(LaunchParams {
                ws: "ws://h".to_string(),
                token: "t".to_string(),
            })
        );
    }

    #[test]
    fn launch_params_none_without_autoconnect() {
        assert_eq!(parse_launch_params("ws=ws://h&token=t"), None);
        assert_eq!(parse_launch_params("ws=ws://h&token=t&autoconnect=0"), None);
    }

    #[test]
    fn launch_params_none_when_ws_or_token_missing_or_empty() {
        assert_eq!(parse_launch_params("token=t&autoconnect=1"), None);
        assert_eq!(parse_launch_params("ws=ws://h&autoconnect=1"), None);
        assert_eq!(parse_launch_params("ws=&token=t&autoconnect=1"), None);
        assert_eq!(parse_launch_params(""), None);
    }

    #[test]
    fn launch_params_ignores_unknown_keys_and_malformed_pairs() {
        assert_eq!(
            parse_launch_params("ws=ws://h&token=t&autoconnect=1&extra=ignored&malformed"),
            Some(LaunchParams {
                ws: "ws://h".to_string(),
                token: "t".to_string(),
            })
        );
    }
}
