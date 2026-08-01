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
//! **This module validates a destination, not a route.** Validation alone does not deliver the
//! guarantee, so the route controls live here too — [`build_http_client`] and [`reject_redirect`],
//! applied by **all four** providers. Redirects must be disabled (reqwest strips `Authorization`
//! only across a host/port change, not across an https→http *scheme* downgrade — and its strip
//! list is fixed, so `x-api-key`/`x-goog-api-key` are never removed at all), a disabled redirect
//! must then be rejected rather than parsed, and the system proxy must be off for `http` bases
//! (otherwise a `HTTP_PROXY` in the environment ships the cleartext request — key included —
//! across the network, which is exactly what the loopback carve-out assumes cannot happen).
//!
//! These pieces live together deliberately: every time one of them was defined in a single
//! provider's file, a sibling provider was left behind and the guarantee silently regressed.

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

/// Name a rejected scheme only when it is a recognized one.
///
/// The "scheme" of a non-URL is just its text before the first `:`, so a credential pasted into
/// the wrong variable (`OPENAI_BASE_URL=$OPENAI_API_KEY`, a transposed compose line) would put its
/// leading segment — `sk-proj-abc` of `sk-proj-abc:123def` — straight into a message that is
/// printed to stderr and thence to CI logs and journald. Anything unrecognized is withheld.
fn nameable_scheme(scheme: &str) -> String {
    const KNOWN: [&str; 8] = [
        "ftp", "file", "ws", "wss", "gopher", "data", "blob", "mailto",
    ];
    if KNOWN.contains(&scheme) {
        scheme.to_string()
    } else {
        "<redacted>".to_string()
    }
}

/// Render a parsed URL as `scheme://host[:port]` only — dropping userinfo, path, query, and
/// fragment, any of which may carry a secret the operator would not want in a log.
fn redact(parsed: &reqwest::Url) -> String {
    // The scheme goes through the same allowlist: for a non-URL it is just the text before the
    // first `:`, which is where a mis-pasted credential's leading segment would land.
    let scheme = match parsed.scheme() {
        s @ ("http" | "https") => s.to_string(),
        other => nameable_scheme(other),
    };
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
///
/// **Returns the NORMALIZED URL, and callers must use that value rather than their input.** The
/// parser lowercases the scheme, trims surrounding whitespace, strips embedded tab/newline, and
/// collapses stray slashes after `scheme:` — so `HTTP://127.0.0.1`, `http:/127.0.0.1`, and
/// `" http://127.0.0.1 "` all validate as loopback `http`, but none of them *starts with* the
/// literal `"http://"`. Any downstream decision made by inspecting the raw string (choosing a
/// proxy policy, say) would therefore disagree with what was validated here. Returning the
/// normalized form is what keeps the two in sync; see [`build_http_client`].
pub fn validate_base_url(base_url: &str) -> Result<String, BaseUrlError> {
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
                scheme: nameable_scheme(other),
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

    Ok(parsed.as_str().to_string())
}

/// Build the HTTP client for a provider endpoint, given an **already-normalized** base URL from
/// [`validate_base_url`].
///
/// - **Redirects are disabled.** reqwest strips `Authorization` only when a redirect changes host
///   or port — *not* when it changes scheme — so a same-host `https`→`http` downgrade would re-send
///   a bearer token in cleartext, and a 307/308 would re-POST the request body (goal plus whatever
///   workspace file contents were gathered) to a host the operator never named. Worse, the strip
///   list is fixed: `x-api-key` / `x-goog-api-key` are **never** removed, so providers using a
///   custom auth header leak on *any* cross-host redirect. No provider endpoint here has a
///   legitimate reason to redirect.
/// - **The system proxy is disabled for `http` bases.** `Client::new()` honors `HTTP_PROXY` /
///   `ALL_PROXY` with no loopback exemption, so a plaintext loopback request would otherwise be
///   shipped across the network — voiding the entire justification for allowing loopback `http`.
///   `https` bases keep proxy support: the proxy sees only a CONNECT tunnel.
///
/// The `http` test parses the URL rather than inspecting a prefix, so it cannot desynchronize from
/// what [`validate_base_url`] decided.
pub(crate) fn build_http_client(base_url: &str) -> reqwest::Client {
    // Phrased as "only a base that parses as https KEEPS the proxy" so the decision fails
    // **closed**: an unparseable base loses proxy support rather than gaining it. (`"http:"`
    // fails to parse, yet `format!("{base}/v1/messages")` still yields a requestable
    // `http://v1/messages` — the base and the final URL do not have the same parseability.)
    let allow_proxy = reqwest::Url::parse(base_url).is_ok_and(|u| u.scheme() == "https");
    let mut builder = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none());
    if !allow_proxy {
        builder = builder.no_proxy();
    }
    // Mirrors `reqwest::Client::new()`, which panics on the same TLS-backend init failure.
    builder
        .build()
        .expect("provider HTTP client failed to initialize")
}

/// Reject a 3xx before the body is parsed.
///
/// [`build_http_client`] disables redirects, so a 3xx arrives as an ordinary response — and
/// `error_for_status()` rejects only 4xx/5xx. Without this, a redirect body would be handed to the
/// response parser: an endpoint answering `302` with valid-looking JSON would have its content
/// accepted as the model's answer, and a provider whose response fields are all `#[serde(default)]`
/// would instead yield a silent empty completion that never triggers the router's remote→local
/// fallback.
///
/// This lives beside `build_http_client` on purpose: the two are a pair, and every provider that
/// takes the client must take this. Keeping it in one provider's file is how the guard and the
/// policy drifted apart in the first place.
pub(crate) fn reject_redirect(resp: &reqwest::Response) -> anyhow::Result<()> {
    if resp.status().is_redirection() {
        anyhow::bail!(
            "provider endpoint returned {} — redirects are disabled for credential safety; \
             point the base URL at the final endpoint instead",
            resp.status()
        );
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
/// `path_suffix` always begins with `/` (it may be a runtime-built `String` — Gemini interpolates
/// its model id into the suffix), so trimming every trailing `/` from the base is sufficient, and
/// makes the function total over pasted input like `https://host///`. A path prefix on the base
/// (`https://host/v1`) is preserved.
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
            assert!(
                validate_base_url(url).is_ok(),
                "expected {url} to be accepted, got {:?}",
                validate_base_url(url)
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
    fn normalizes_accepted_urls_so_the_scheme_cannot_be_misread_downstream() {
        // Each of these validates as loopback `http`, yet NONE starts with the literal "http://".
        // A downstream proxy decision made by string prefix would therefore skip `no_proxy()` and
        // ship the cleartext request — key included — to an operator's HTTP_PROXY. The normalized
        // return value is what keeps that decision in sync with what was validated here.
        for raw in [
            "http:/127.0.0.1:9",
            "HTTP://127.0.0.1:9",
            " http://127.0.0.1:9",
            "http:127.0.0.1:9",
        ] {
            let normalized = validate_base_url(raw)
                .unwrap_or_else(|e| panic!("{raw} should validate as loopback http, got {e:?}"));
            assert!(
                normalized.starts_with("http://"),
                "{raw} normalized to {normalized}, which still hides the scheme"
            );
            assert!(
                reqwest::Url::parse(&normalized).is_ok_and(|u| u.scheme() == "http"),
                "{normalized} must parse back as http"
            );
        }
    }

    #[test]
    fn normalization_strips_surrounding_whitespace() {
        // Otherwise join_url would append onto a base with an interior space, which reqwest
        // percent-encodes into a mis-targeted path.
        let normalized = validate_base_url(" https://api.openai.com ").unwrap();
        assert!(!normalized.contains(' '), "got {normalized}");
        assert!(normalized.starts_with("https://api.openai.com"));
    }

    #[test]
    fn unrecognized_schemes_are_not_echoed() {
        // The "scheme" of a non-URL is just the text before the first ':', so a credential pasted
        // into the wrong env var would otherwise have its leading segment printed to stderr.
        for raw in ["sk-proj-abc123:secret", "ghp-tokenvalue:more"] {
            let msg = validate_base_url(raw).unwrap_err().to_string();
            assert!(!msg.contains("sk-proj-abc123"), "leaked: {msg}");
            assert!(!msg.contains("ghp-tokenvalue"), "leaked: {msg}");
            assert!(
                msg.contains("<redacted>"),
                "expected redaction marker: {msg}"
            );
        }
        // A genuinely recognizable scheme is still named, since that is useful and not a secret.
        let msg = validate_base_url("ftp://host/x").unwrap_err().to_string();
        assert!(msg.contains("ftp"), "got {msg}");
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
