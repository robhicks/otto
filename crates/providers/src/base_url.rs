//! The trust boundary for operator-supplied provider base URLs.
//!
//! `OPENAI_BASE_URL` / `DEEPSEEK_BASE_URL` let an operator retarget the OpenAI-compatible
//! providers at a proxy, a gateway, or a test server. Whatever they name receives the provider's
//! API key as a `Bearer` header, so the value is validated before a provider is ever constructed:
//! `https` always, and plain `http` **only** to a loopback host.
//!
//! The loopback carve-out exists because every provider test builds against wiremock's
//! `server.uri()`, which is `http://127.0.0.1:<port>`. Traffic to loopback never leaves the
//! machine, so a key sent there is not exposed to the network.
//!
//! Host matching is exact equality against the parsed host — never a suffix match, and never a DNS
//! resolution. A name that merely *resolves* to 127.0.0.1 (DNS rebinding) is therefore rejected,
//! and `localhost.evil.com` does not pass by virtue of containing `localhost`.
//!
//! The provider's API key is never in scope in this module. The *base URL itself* can be a
//! credential, though — `https://gw/?api-key=…` or `https://user:pw@gw/` — so every error carries
//! only a redacted `scheme://host:port`, never the raw string. A base URL with userinfo, a query,
//! or a fragment is refused outright: none is meaningful on a base, and refusing them is what lets
//! [`join_url`] be a safe string concatenation rather than a partial one.
//!
//! **This module validates a destination, not a route.** Two client-level settings are required
//! for the guarantee to actually hold, and both live in `openai_compatible.rs`:
//! redirects must be disabled (reqwest strips `Authorization` only across a host/port change, not
//! across an https→http *scheme* downgrade), and the system proxy must be off for `http` bases
//! (otherwise a `HTTP_PROXY` in the environment ships the cleartext request — key included —
//! across the network, which is exactly what the loopback carve-out assumes cannot happen).

use std::fmt;

/// Why a base URL was refused.
///
/// Every variant carries a **redacted** rendering (`scheme://host:port`) rather than the operator's
/// raw string: a base URL can itself hold a secret in its userinfo or query, and these values are
/// printed to stderr, where they reach CI logs and journald.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BaseUrlError {
    /// Not a URL at all. Note `https://` and `http://` (empty host) land here too — the `url`
    /// crate rejects them at parse time rather than yielding a hostless URL. Carries no detail
    /// at all, since an unparseable string cannot be safely redacted.
    Unparseable,
    /// A scheme other than `http`/`https` (`ftp`, `file`, `ws`, …).
    UnsupportedScheme { redacted: String, scheme: String },
    /// Plain `http` to a host that is not loopback — this is the case that would have put an API
    /// key on the wire in cleartext.
    InsecureScheme(String),
    /// Userinfo (`user:pass@`) is present. reqwest would turn it into an `Authorization: Basic`
    /// header that collides with the provider's own `Bearer` header, and it is a common place to
    /// hide a token.
    HasUserinfo(String),
    /// A query or fragment is present. A *base* URL has neither, and appending a path suffix after
    /// a query silently mis-targets the request (`https://gw/?t=x` + `/v1/chat` → `…?t=x/v1/chat`).
    HasQueryOrFragment(String),
    /// A URL with no host component. Unreachable for `http`/`https` (the `url` crate guarantees a
    /// non-empty host for them); present only so the host match is total rather than an `unwrap`.
    MissingHost(String),
}

impl fmt::Display for BaseUrlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unparseable => write!(
                f,
                "the value is not a valid URL (omitted here, since an unparseable string cannot \
                 be safely redacted and may contain a secret)"
            ),
            Self::UnsupportedScheme { redacted, scheme } => write!(
                f,
                "'{redacted}' uses unsupported scheme '{scheme}'; expected https (or http to \
                 localhost)"
            ),
            Self::InsecureScheme(redacted) => write!(
                f,
                "'{redacted}' uses plain http to a non-loopback host, which would send the API key \
                 in cleartext; use https (plain http is allowed only for localhost/127.0.0.1/[::1])"
            ),
            Self::HasUserinfo(redacted) => write!(
                f,
                "'{redacted}' carries embedded credentials (user:pass@); remove them — they would \
                 become a Basic auth header colliding with the provider's Bearer token"
            ),
            Self::HasQueryOrFragment(redacted) => write!(
                f,
                "'{redacted}' carries a query or fragment; a base URL must have neither, or the \
                 appended request path would be silently mis-targeted"
            ),
            Self::MissingHost(redacted) => write!(f, "'{redacted}' has no host component"),
        }
    }
}

impl std::error::Error for BaseUrlError {}

/// Render a parsed URL as `scheme://host[:port]` only — dropping userinfo, path, query, and
/// fragment, any of which may carry a secret the operator would not want in a log.
fn redact(parsed: &reqwest::Url) -> String {
    let scheme = parsed.scheme();
    let host = parsed.host_str().unwrap_or("<no-host>");
    match parsed.port() {
        Some(port) => format!("{scheme}://{host}:{port}"),
        None => format!("{scheme}://{host}"),
    }
}

/// Validate an operator-supplied provider base URL before any API key is sent to it.
///
/// Accepts any `https://` URL, and `http://` only when the host is loopback (`localhost`, an IPv4
/// address in `127.0.0.0/8`, or `[::1]`).
pub fn validate_base_url(base_url: &str) -> Result<(), BaseUrlError> {
    let parsed = reqwest::Url::parse(base_url).map_err(|_| BaseUrlError::Unparseable)?;
    let redacted = redact(&parsed);

    // Scheme first: it decides whether the key can travel at all.
    match parsed.scheme() {
        "https" => {}
        "http" => {
            let host = parsed
                .host_str()
                .ok_or_else(|| BaseUrlError::MissingHost(redacted.clone()))?;
            if !is_loopback_host(host) {
                return Err(BaseUrlError::InsecureScheme(redacted));
            }
        }
        other => {
            return Err(BaseUrlError::UnsupportedScheme {
                redacted,
                scheme: other.to_string(),
            });
        }
    }

    // Shape checks. These are what make `join_url`'s plain concatenation total rather than
    // partial, and they remove reqwest's userinfo -> `Authorization: Basic` conversion.
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(BaseUrlError::HasUserinfo(redacted));
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(BaseUrlError::HasQueryOrFragment(redacted));
    }

    Ok(())
}

/// Loopback iff the host is literally `localhost`, or an IP the standard library calls loopback.
/// Deliberately no DNS resolution and no suffix matching — a name that merely resolves to
/// 127.0.0.1 must not pass, and `localhost.evil.com` must not pass either.
///
/// `host_str()` has already been normalized by `url`: the host is lowercased, and a decimal or
/// hex IPv4 literal (`2130706433`) is rewritten to dotted form, so both reach the `IpAddr` parse
/// below in canonical shape. IPv6 hosts keep their surrounding brackets, which are stripped here.
///
/// `to_canonical()` folds an IPv4-mapped IPv6 address (`::ffff:127.0.0.1`) down to its IPv4 form
/// before the loopback test. Without it that spelling — which the OS routes to 127.0.0.1 like any
/// other loopback address — would be refused, because `Ipv6Addr::is_loopback` matches only `::1`.
/// Folding cannot widen the result: a mapped non-loopback address (`::ffff:169.254.169.254`)
/// canonicalizes to a non-loopback IPv4 and is still refused.
fn is_loopback_host(host: &str) -> bool {
    if host == "localhost" {
        return true;
    }
    let unbracketed = host.strip_prefix('[').and_then(|h| h.strip_suffix(']'));
    unbracketed
        .unwrap_or(host)
        .parse::<std::net::IpAddr>()
        .is_ok_and(|ip| ip.to_canonical().is_loopback())
}

/// Join a base URL and a path suffix with exactly one separator.
///
/// `path_suffix` is always a `&'static str` beginning with `/`, so trimming every trailing `/`
/// from the base is sufficient — and makes the function total over pasted input like
/// `https://host///`. A path prefix on the base (`https://host/v1`) is preserved.
pub(crate) fn join_url(base_url: &str, path_suffix: &str) -> String {
    format!("{}{}", base_url.trim_end_matches('/'), path_suffix)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_https_and_loopback_http() {
        for url in [
            "https://api.openai.com",
            "https://api.deepseek.com",
            "https://api.openai.com/v1/",
            "http://127.0.0.1:8080",
            "http://localhost:1234",
            "http://[::1]:8080",
            // Decimal-encoded 127.0.0.1: genuinely loopback, so genuinely accepted.
            "http://2130706433",
            // IPv4-mapped IPv6 loopback: the OS routes this to 127.0.0.1, so it is loopback.
            "http://[::ffff:127.0.0.1]:8080",
            // `url` lowercases the host at parse time, so the equality check is case-safe.
            "http://LOCALHOST",
            // NOT hostless: this parses with host `foo`, so the https rule accepts it.
            "https:///foo",
        ] {
            assert_eq!(
                validate_base_url(url),
                Ok(()),
                "expected {url} to be accepted"
            );
        }
    }

    #[test]
    fn rejects_cleartext_http_to_non_loopback_hosts() {
        for url in [
            "http://api.openai.com",
            // Contains "localhost" but is not equal to it — suffix matching would wrongly accept.
            "http://localhost.evil.com",
            // Trailing dot is a distinct host string from `localhost`.
            "http://localhost.",
            // Cloud instance-metadata endpoint; not loopback.
            "http://169.254.169.254",
            "http://10.0.0.5",
            "http://192.168.1.10",
            // Mapping a non-loopback address into IPv6 must not launder it.
            "http://[::ffff:169.254.169.254]",
        ] {
            assert!(
                matches!(validate_base_url(url), Err(BaseUrlError::InsecureScheme(_))),
                "expected {url} to be rejected as insecure, got {:?}",
                validate_base_url(url)
            );
        }
    }

    #[test]
    fn rejects_embedded_credentials() {
        // reqwest turns userinfo into an `Authorization: Basic` header, which would collide with
        // the provider's own Bearer token — and userinfo is a common place to hide a secret.
        for url in [
            "https://user:pass@gw.example.com/v1",
            "https://token@gw.example.com",
            "http://user:pass@127.0.0.1:8080",
        ] {
            assert!(
                matches!(validate_base_url(url), Err(BaseUrlError::HasUserinfo(_))),
                "expected {url} to be rejected for embedded credentials"
            );
        }
    }

    #[test]
    fn rejects_query_or_fragment_on_a_base() {
        // Appending a path suffix after a query silently mis-targets the request; rejecting these
        // is what makes join_url's plain concatenation safe by construction.
        for url in [
            "https://gw.example.com/v1?tenant=x",
            "https://gw.example.com/?api-key=SECRET",
            "https://gw.example.com/v1#frag",
        ] {
            assert!(
                matches!(
                    validate_base_url(url),
                    Err(BaseUrlError::HasQueryOrFragment(_))
                ),
                "expected {url} to be rejected for a query/fragment"
            );
        }
    }

    #[test]
    fn errors_redact_the_url_and_never_echo_a_secret() {
        // A rejected value is printed to stderr, so anything beyond scheme://host:port — where a
        // token can hide — must not survive into the message.
        let cases = [
            "https://user:s3cret@gw.example.com/v1",
            "https://gw.example.com/v1?api-key=s3cret",
            "http://evil.example.com/path?token=s3cret",
        ];
        for url in cases {
            let msg = validate_base_url(url).unwrap_err().to_string();
            assert!(
                !msg.contains("s3cret"),
                "error for {url} leaked the secret: {msg}"
            );
        }
        // An unparseable value is not echoed at all, since it cannot be safely redacted.
        let msg = validate_base_url("::::not-a-url::::s3cret")
            .unwrap_err()
            .to_string();
        assert!(!msg.contains("s3cret"), "unparseable error leaked: {msg}");
    }

    #[test]
    fn rejects_non_http_schemes() {
        for (url, scheme) in [
            ("ftp://host/x", "ftp"),
            ("file:///etc/passwd", "file"),
            ("ws://host", "ws"),
        ] {
            assert!(
                matches!(
                    validate_base_url(url),
                    Err(BaseUrlError::UnsupportedScheme { scheme: ref s, .. }) if s == scheme
                ),
                "expected {url} to be rejected for scheme {scheme}, got {:?}",
                validate_base_url(url)
            );
        }
    }

    #[test]
    fn rejects_unparseable_input() {
        // `https://` / `http://` have an empty host, which `url` rejects at parse time — so they
        // surface as Unparseable rather than as a distinct hostless variant.
        for url in ["not a url", "", "https://", "http://"] {
            assert_eq!(
                validate_base_url(url),
                Err(BaseUrlError::Unparseable),
                "expected {url:?} to be rejected as unparseable"
            );
        }
    }

    #[test]
    fn insecure_scheme_error_names_the_host_and_the_expectation() {
        let msg = validate_base_url("http://api.openai.com")
            .unwrap_err()
            .to_string();
        assert!(msg.contains("http://api.openai.com"), "got: {msg}");
        assert!(msg.contains("https"), "got: {msg}");
    }

    #[test]
    fn join_url_produces_exactly_one_separator() {
        for suffix in ["/v1/chat/completions", "/chat/completions"] {
            // No trailing slash on the base.
            assert_eq!(
                join_url("https://host", suffix),
                format!("https://host{suffix}")
            );
            // One trailing slash.
            assert_eq!(
                join_url("https://host/", suffix),
                format!("https://host{suffix}")
            );
            // Several trailing slashes.
            assert_eq!(
                join_url("https://host///", suffix),
                format!("https://host{suffix}")
            );
            // A path prefix on the base is preserved (Azure-style deployments).
            assert_eq!(
                join_url("https://host/v1", suffix),
                format!("https://host/v1{suffix}")
            );
            assert_eq!(
                join_url("https://host/v1/", suffix),
                format!("https://host/v1{suffix}")
            );
        }
    }

    #[test]
    fn join_url_never_doubles_the_slash_after_the_authority() {
        for base in ["https://host", "https://host/", "https://host///"] {
            let joined = join_url(base, "/v1/chat/completions");
            let after_scheme = joined.strip_prefix("https://").unwrap();
            assert!(
                !after_scheme.contains("//"),
                "{joined} contains a doubled slash"
            );
        }
    }
}
