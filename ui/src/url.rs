//! Pure, browser-free helpers. Unit-tested with plain `cargo test` on the host
//! (no wasm, no DOM) — this is the determinism seam for the UI's logic.

/// Build the `/ws` connection URL. `session`/`last_seq` are appended only when reconnecting.
pub fn build_ws_url(
    base: &str,
    token: &str,
    session: Option<&str>,
    last_seq: Option<u64>,
) -> String {
    let base = base.trim_end_matches('/');
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
}
