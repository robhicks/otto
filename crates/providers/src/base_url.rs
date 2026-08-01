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
//! No API key is ever in scope in this module, so no error or log line here can leak one.

use std::fmt;

/// Why a base URL was refused. Carries the offending URL for the operator's benefit; never a key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BaseUrlError {
    /// Not a URL at all. Note `https://` and `http://` (empty host) land here too — the `url`
    /// crate rejects them at parse time rather than yielding a hostless URL.
    Unparseable(String),
    /// A scheme other than `http`/`https` (`ftp`, `file`, `ws`, …).
    UnsupportedScheme { url: String, scheme: String },
    /// Plain `http` to a host that is not loopback — this is the case that would have put an API
    /// key on the wire in cleartext.
    InsecureScheme(String),
    /// A URL with no host component. Unreachable for `http`/`https` (the `url` crate guarantees a
    /// non-empty host for them); present only so the host match is total rather than an `unwrap`.
    MissingHost(String),
}

impl fmt::Display for BaseUrlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unparseable(url) => {
                write!(f, "'{url}' is not a valid URL")
            }
            Self::UnsupportedScheme { url, scheme } => write!(
                f,
                "'{url}' uses unsupported scheme '{scheme}'; expected https (or http to localhost)"
            ),
            Self::InsecureScheme(url) => write!(
                f,
                "'{url}' uses plain http to a non-loopback host, which would send the API key in \
                 cleartext; use https (plain http is allowed only for localhost/127.0.0.1/[::1])"
            ),
            Self::MissingHost(url) => write!(f, "'{url}' has no host component"),
        }
    }
}

impl std::error::Error for BaseUrlError {}

/// Validate an operator-supplied provider base URL before any API key is sent to it.
///
/// Accepts any `https://` URL, and `http://` only when the host is loopback (`localhost`, an IPv4
/// address in `127.0.0.0/8`, or `[::1]`).
pub fn validate_base_url(base_url: &str) -> Result<(), BaseUrlError> {
    let parsed = reqwest::Url::parse(base_url)
        .map_err(|_| BaseUrlError::Unparseable(base_url.to_string()))?;

    match parsed.scheme() {
        "https" => Ok(()),
        "http" => {
            let host = parsed
                .host_str()
                .ok_or_else(|| BaseUrlError::MissingHost(base_url.to_string()))?;
            if is_loopback_host(host) {
                Ok(())
            } else {
                Err(BaseUrlError::InsecureScheme(base_url.to_string()))
            }
        }
        other => Err(BaseUrlError::UnsupportedScheme {
            url: base_url.to_string(),
            scheme: other.to_string(),
        }),
    }
}

/// Loopback iff the host is literally `localhost`, or an IP the standard library calls loopback.
/// Deliberately no DNS resolution and no suffix matching — a name that merely resolves to
/// 127.0.0.1 must not pass, and `localhost.evil.com` must not pass either.
///
/// `host_str()` has already been normalized by `url`: the host is lowercased, and a decimal or
/// hex IPv4 literal (`2130706433`) is rewritten to dotted form, so both reach the `IpAddr` parse
/// below in canonical shape. IPv6 hosts keep their surrounding brackets, which are stripped here.
fn is_loopback_host(host: &str) -> bool {
    if host == "localhost" {
        return true;
    }
    let unbracketed = host.strip_prefix('[').and_then(|h| h.strip_suffix(']'));
    unbracketed
        .unwrap_or(host)
        .parse::<std::net::IpAddr>()
        .is_ok_and(|ip| ip.is_loopback())
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
        ] {
            assert_eq!(
                validate_base_url(url),
                Err(BaseUrlError::InsecureScheme(url.to_string())),
                "expected {url} to be rejected as insecure"
            );
        }
    }

    #[test]
    fn rejects_non_http_schemes() {
        for (url, scheme) in [
            ("ftp://host/x", "ftp"),
            ("file:///etc/passwd", "file"),
            ("ws://host", "ws"),
        ] {
            assert_eq!(
                validate_base_url(url),
                Err(BaseUrlError::UnsupportedScheme {
                    url: url.to_string(),
                    scheme: scheme.to_string()
                }),
                "expected {url} to be rejected for its scheme"
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
                Err(BaseUrlError::Unparseable(url.to_string())),
                "expected {url:?} to be rejected as unparseable"
            );
        }
    }

    #[test]
    fn error_display_never_contains_a_key_and_names_the_url() {
        let e = validate_base_url("http://api.openai.com").unwrap_err();
        let msg = e.to_string();
        assert!(msg.contains("http://api.openai.com"));
        assert!(msg.contains("https"));
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
