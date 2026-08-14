//! Pure, browser-free helpers. Unit-tested with plain `cargo test` on the host
//! (no wasm, no DOM) — this is the determinism seam for the UI's logic.

/// Build the `/ws` connection URL. `session`/`last_seq` are appended only when reconnecting.
///
/// No credential rides here: the serve protocol no longer reads a `?token=` query parameter
/// (spec §6.4) — the desktop sidecar serves `--single-user` (nothing to send), and browser
/// authentication happens post-upgrade via `Login`/`Attach` frames in slice 2. So the URL is just
/// the endpoint plus optional `session`/`last_seq` replay markers.
///
/// [`redact_token`] still exists and still guards every transport diagnostic (`SeamError::new`):
/// redaction is defense-in-depth for any credential-bearing text a future call site or a browser
/// API echo may surface, even though this function no longer produces any.
pub fn build_ws_url(base: &str, session: Option<&str>, last_seq: Option<u64>) -> String {
    // Normalize the base: tolerate a trailing slash and a base that already ends in
    // `/ws` (e.g. the endpoint URL `otto serve` prints), so we never produce `/ws/ws`.
    let base = base.trim_end_matches('/');
    let base = base.strip_suffix("/ws").unwrap_or(base);
    let mut url = format!("{base}/ws");
    let mut query: Vec<String> = Vec::new();
    if let Some(s) = session {
        query.push(format!("session={}", urlencoding::encode(s)));
    }
    if let Some(seq) = last_seq {
        query.push(format!("last_seq={seq}"));
    }
    if !query.is_empty() {
        url.push('?');
        url.push_str(&query.join("&"));
    }
    url
}

/// Replace the value of every `token=` query parameter in a diagnostic string with `<redacted>`.
///
/// The leak this closed originally: `build_ws_url` put the bearer token in the query string, and
/// the browser's `WebSocket::new` rejects a malformed URL with a `SyntaxError` whose text **quotes
/// the offending URL in full**. That string became a `transport::SeamError`, so it flowed straight
/// into `client_error_row(ClientText::Passthrough(..))` and rendered in the event log — a surface
/// this crate's own docs describe as "their audience is a bug report", i.e. exactly the text a
/// user is most likely to copy into an issue.
///
/// `build_ws_url` no longer emits a `token=` (spec §6.4; slice 2 moves credentials to post-upgrade
/// frames), so the specific URL leak is gone — but the function stays, guarding the whole seam by
/// construction: `transport::SeamError::new` redacts every diagnostic on both targets, so whatever
/// *future* call site hands it a credential-bearing URL (a `Login`/`Refresh` endpoint, a browser
/// API echo of a connect attempt, an RPC URL) cannot leak through the event log by accident. The
/// scan is cheap and value-scoped, so keeping it on the seam costs nothing while the credential
/// stays anywhere in this crate's vocabulary.
///
/// Redaction rather than a fixed authored message, deliberately: the host, port, path, `session`,
/// and `last_seq` are what make "the URL was rejected" actionable, and replacing the whole
/// diagnostic would trade a real leak for a useless error. Only the secret goes.
///
/// The scan is value-scoped — it stops at the first `&`/`#`, quote, or whitespace — so trailing
/// query parameters and any surrounding prose survive. Over-matching (a key that merely *ends* in
/// `token=`) redacts something harmless; under-matching would leak, so the bias is deliberate.
///
/// Its production caller is `transport::SeamError::new` — the seam's single constructor — so
/// every transport diagnostic is redacted by construction on **both** targets, rather than at
/// hand-audited call sites. That is why there is no longer a dead-code allow here: the seam type
/// is compiled under every feature combination, including `--no-default-features`.
///
/// It lives in this browser-free module rather than beside a caller so it stays host-testable —
/// the tests below are the ones that actually pin the redaction, and
/// `transport::tests::new_redacts_a_bearer_token_whatever_the_call_site_formats` pins that the
/// constructor applies it.
pub fn redact_token(diagnostic: &str) -> String {
    const KEY: &str = "token=";
    const MASK: &str = "<redacted>";
    let mut out = String::with_capacity(diagnostic.len());
    let mut rest = diagnostic;
    while let Some(at) = rest.find(KEY) {
        out.push_str(&rest[..at + KEY.len()]);
        let value = &rest[at + KEY.len()..];
        // Every delimiter is ASCII, so this byte index is always a char boundary.
        let end = value
            .find(|c: char| matches!(c, '&' | '#' | '"' | '\'' | ')') || c.is_whitespace())
            .unwrap_or(value.len());
        out.push_str(MASK);
        rest = &value[end..];
    }
    out.push_str(rest);
    out
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

/// The desktop wrapper's auto-connect bootstrap (sub-project G): the local sidecar's WS base URL
/// and a connect credential, carried as query params on the webview's initial navigation
/// (`desktop/src-tauri/src/launch.rs`'s `build_launch_url` is the writer side of this contract).
///
/// `token` is **empty on the desktop path** — the sidecar serves `--single-user`, so the webview
/// connects bare (see `desktop_boot::boot`). The field stays because the *web* target's
/// `parse_launch_params` still reads a token from an external bootstrap URL; slice 2 replaces
/// both with the post-upgrade login flow.
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
    fn url_without_reconnect_has_no_credential() {
        let u = build_ws_url("ws://127.0.0.1:8787", None, None);
        assert_eq!(u, "ws://127.0.0.1:8787/ws");
    }

    #[test]
    fn url_trims_trailing_slash_and_appends_no_token() {
        let u = build_ws_url("ws://host/", None, None);
        assert_eq!(u, "ws://host/ws");
    }

    #[test]
    fn url_with_reconnect_appends_session_and_seq() {
        let u = build_ws_url("ws://h", Some("sess-1"), Some(12));
        assert_eq!(u, "ws://h/ws?session=sess-1&last_seq=12");
    }

    #[test]
    fn redact_token_removes_the_secret_and_keeps_the_diagnostic_useful() {
        // The exact shape a browser `SyntaxError` carries: the whole URL, quoted, inside prose.
        // `build_ws_url` no longer emits `token=` (spec §6.4), so the URL is hand-assembled to
        // exercise the defensive scan against a future credential-bearing call site.
        let raw = "ws://h/ws?token=s3cr3t-bearer&session=sess-1&last_seq=12";
        let err = format!(
            "JsValue(SyntaxError: Failed to construct 'WebSocket': The URL '{raw}' is invalid.)"
        );
        let safe = redact_token(&err);
        assert!(!safe.contains("s3cr3t-bearer"), "token survived: {safe}");
        // Everything that makes the error actionable is still there.
        assert!(safe.contains("token=<redacted>"), "no mask in: {safe}");
        assert!(safe.contains("ws://h/ws"), "host/path lost: {safe}");
        assert!(safe.contains("session=sess-1"), "session lost: {safe}");
        assert!(safe.contains("last_seq=12"), "last_seq lost: {safe}");
        assert!(safe.contains("SyntaxError"), "prose lost: {safe}");
    }

    #[test]
    fn redact_token_stops_at_the_value_boundary() {
        assert_eq!(redact_token("token=abc&x=1"), "token=<redacted>&x=1");
        assert_eq!(redact_token("token=abc#frag"), "token=<redacted>#frag");
        assert_eq!(
            redact_token("url is token=abc here"),
            "url is token=<redacted> here"
        );
        assert_eq!(redact_token("'token=abc'"), "'token=<redacted>'");
        // Last parameter, nothing after it.
        assert_eq!(
            redact_token("ws://h/ws?token=abc"),
            "ws://h/ws?token=<redacted>"
        );
        // An empty value is still masked rather than left as a bare `token=`.
        assert_eq!(redact_token("token=&x=1"), "token=<redacted>&x=1");
        // More than one occurrence (a retry log, say) — every one goes.
        assert_eq!(
            redact_token("token=a then token=b"),
            "token=<redacted> then token=<redacted>"
        );
    }

    #[test]
    fn redact_token_leaves_token_free_text_untouched() {
        assert_eq!(redact_token("socket closed"), "socket closed");
        assert_eq!(redact_token(""), "");
        // The word "token" without a `=` is not a parameter.
        assert_eq!(redact_token("bad token supplied"), "bad token supplied");
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
            build_ws_url("ws://127.0.0.1:8787/ws", None, None),
            "ws://127.0.0.1:8787/ws"
        );
        // trailing slash + /ws both handled
        assert_eq!(build_ws_url("ws://h/ws/", None, None), "ws://h/ws");
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
