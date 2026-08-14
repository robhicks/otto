//! `SeamError` lives in its own module so its private field is unreachable from the per-target
//! transport impls.
//!
//! This placement is load-bearing, and the reason is a Rust visibility subtlety that is easy to
//! get wrong twice: a private field is visible to the declaring module **and all its
//! descendants**. With `SeamError` declared in `transport/mod.rs`, `transport::web` and
//! `transport::desktop` were descendants, so `SeamError(format!("{e:?}"))` compiled there —
//! constructing one directly, bypassing `new` and therefore bypassing redaction. A security
//! review demonstrated it with a repro that printed a leaked token.
//!
//! Declared here instead, `web` and `desktop` are **siblings** of this module rather than
//! descendants, so the field is unnameable from them, while `pub(in crate::transport) fn new`
//! stays callable (they are still inside `crate::transport`). That turns the guarantee below from
//! a convention the reviewer has to police into one the compiler does.

use crate::net::url::redact_token;

/// A failure diagnostic that reached the app through the transport seam.
///
/// # What this type guarantees, precisely
///
/// **"Minted under `transport/`, through `new`"** — a *location* claim, not a provenance one.
/// `new` is the only way to build one: the field is private to this module, and `transport::web`
/// and `transport::desktop` are siblings, not descendants (see the module doc above). So `net/`,
/// `app.rs`, `components/`, and `desktop_boot.rs` can hold, compare, and render a `SeamError` but
/// can never fabricate one, and the per-target impls cannot sidestep the constructor.
///
/// That is what makes `ClientText::Passthrough(SeamError)` a boundary rather than a comment, and
/// it collapses the review surface for "is this text wrongly escaping localization?" from the
/// whole crate to three files.
///
/// It does **not** guarantee the text is server-authored. Two ways it is not:
/// - The workspace-RPC path returns a server-sent `WorkspaceResponse::Error` payload as a seam
///   error — server-authored, which is the direction you would expect.
/// - Several diagnostics in this subtree are crate-authored English (`"socket closed"`,
///   `"workspace rpc failed: HTTP {status}"`, `"unexpected response to List: …"`,
///   `"unexpected response to Read: …"`, and the no-feature fallback). They render untranslated in
///   every locale by design (i18n spec §2), but the rule that keeps *new* interface copy out of
///   `transport/` is still review, not the compiler. Do not write user-facing instructions here.
///
/// # Why the redaction lives in the constructor
///
/// The leak it was built to close: `build_ws_url` put the bearer token in a query parameter, and a
/// rejected URL comes back from the browser quoting the URL in full. That URL is gone now (spec
/// §6.4 — slice 2 moves credentials to post-upgrade frames), but the structural guarantee is what
/// stays worth keeping: redacting at each call site made "did this one remember?" a per-site
/// review question that a source-scanning test could only approximate — and it left the desktop
/// transport uncovered entirely. Because `new` is the only constructor, it is the one place that
/// makes the property structural: **no diagnostic can leave this seam carrying a `token=` query
/// parameter, whatever a future call site formats.** `redact_token` is idempotent, so a call site
/// that also redacts is harmless.
///
/// Scope, stated exactly: `redact_token` recognizes the `token=<value>` *query* form. The same
/// secret also travels as an `Authorization: Bearer …` header on the workspace RPC. No diagnostic
/// carries headers today (neither HTTP client's error Display includes them), so there is no leak
/// — but a future site that formats a request dump would not be covered by this constructor, and
/// should not assume it is.
///
/// # Deliberate omissions
///
/// No `From<String>`/`From<&str>`, no `Default`, no `std::error::Error`: each is a public
/// constructor by another name, and `Error` additionally pulls this type into `?`-conversion
/// chains that invite one.
///
/// # Known limit
///
/// The visibility is scoped to *this crate's* `transport` module. If `transport/` is ever
/// extracted into its own crate, `pub(in crate::transport)` becomes crate-wide there — the
/// non-enforcing shape this type exists to avoid — and no test would notice. Re-derive the
/// boundary if that move happens.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SeamError(String);

impl SeamError {
    /// Mint a diagnostic, redacting any bearer token it carries.
    ///
    /// `pub(in crate::transport)` rather than plain private because `transport::{web,desktop}` are
    /// siblings of this module, not descendants, so plain private would not reach them — the same
    /// visibility rule that makes the field above unreachable is what makes this modifier
    /// necessary here. (In the type's previous home in `transport/mod.rs` it was a no-op.)
    pub(in crate::transport) fn new(detail: impl Into<String>) -> Self {
        Self(redact_token(&detail.into()))
    }

    /// The diagnostic text, for rendering. Read-only by construction.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Mint one in a test that exercises a *consumer* of the seam rather than the seam itself
    /// (e.g. `net::view_model`'s row-rendering tests). `cfg(test)` so no production path reaches
    /// it, and it delegates to `new` so even test values are redacted.
    #[cfg(test)]
    pub fn for_test(detail: impl Into<String>) -> Self {
        Self::new(detail)
    }
}

/// Deliberately `Display` but NOT `std::error::Error`.
///
/// No production caller uses it today — `render_row` goes through `as_str()`. It is here because
/// `Display` is what a diagnostic newtype owes an `eprintln!`/`log` call site, and because writing
/// it explicitly documents where the line is drawn: `Error` is the trait that would pull this type
/// into `?`-conversion chains and invite a `From<String>` impl, which is a public constructor by
/// another name and would undo the boundary above.
impl std::fmt::Display for SeamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::SeamError;

    /// `SeamError` must expose exactly one production constructor.
    ///
    /// The compiler is the real enforcement — this module's placement makes the field unreachable
    /// from the per-target impls, and `new` is the only door. This test catches a later
    /// *widening*. It can afford to be strict because it scans one small, dedicated file:
    /// everything that could add a constructor to this type lives here.
    ///
    /// **It scans only the source ABOVE this test module**, which is the fix for the failure mode
    /// that broke its two previous versions. A source-scanning test that reads its own file will
    /// match its own needles: the first version's `From<String>` assertion fired on the line
    /// spelling it out, and later a doc comment mentioning a derive tripped the derive check. By
    /// slicing the production half off first, the test's own text is simply not in the haystack —
    /// no `concat!` splitting, and prose here is free to name any syntax it likes.
    ///
    /// Whitespace is collapsed before scanning so a stacked attribute or a header split across
    /// lines reads identically to a single-line one — the other gap reviewers demonstrated.
    #[test]
    fn seam_error_has_no_crate_wide_constructor() {
        let src = include_str!("seam_error.rs");
        // Scan the source ABOVE this test module, with comments stripped: the test's own text is
        // then not in the haystack at all, and prose above cannot be mistaken for code.
        let production = src
            .split("#[cfg(test)]\nmod tests {")
            .next()
            .expect("source above the test module");
        let code: String = production
            .lines()
            .map(str::trim)
            .filter(|l| !l.starts_with("//"))
            .collect::<Vec<_>>()
            .join(" ");

        // 1. A private tuple field is what makes `new` the only door.
        assert!(
            code.contains("pub struct SeamError(String);"),
            "SeamError's tuple field is no longer private"
        );

        // 2. No derive may synthesize a constructor — every derive attribute, not just the first,
        //    so a stacked one cannot hide.
        for attr in code.split("#[derive(").skip(1) {
            let list = attr.split(')').next().unwrap_or("");
            assert!(
                !list.contains("Default"),
                "derive(Default) synthesizes a public SeamError constructor"
            );
        }

        // 3. The inherent impl block, bounded by brace depth rather than by a text needle, must
        //    hold exactly the three known functions.
        let open = code
            .find("impl SeamError {")
            .expect("the inherent impl block");
        let body = &code[open + "impl SeamError {".len()..];
        let mut depth = 1usize;
        let mut end = body.len();
        for (i, ch) in body.char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = i;
                        break;
                    }
                }
                _ => {}
            }
        }
        let block = &body[..end];
        assert_eq!(
            block.matches(" fn ").count(),
            3,
            "SeamError gained or lost a function — a new one must be justified here"
        );
        for expected in [
            "pub(in crate::transport) fn new(",
            "pub fn as_str(",
            "pub fn for_test(",
        ] {
            assert!(block.contains(expected), "missing or reshaped: {expected}");
        }

        // 4. Ungating `for_test` alone would make it a crate-wide public constructor.
        assert!(
            block.contains("#[cfg(test)] pub fn for_test"),
            "SeamError::for_test lost its cfg(test) gate — it is now a public constructor"
        );

        // 5. `Display` is the only trait impl allowed. `From`, `FromStr`, and `Deref` are all
        //    constructors by another name.
        let segments: Vec<&str> = code.split("for SeamError").collect();
        for head in &segments[..segments.len().saturating_sub(1)] {
            let trait_path = head.rsplit("impl ").next().unwrap_or("");
            assert!(
                trait_path.contains("std::fmt::Display"),
                "disallowed trait impl on the seam type: {trait_path}"
            );
        }
    }

    /// `new` redacts, so every diagnostic leaving the seam is safe by construction.
    ///
    /// This replaces a source scan asserting the two `web.rs` sites called `redact_token` by hand.
    /// That scan was evadable (it keyed on the literal `{e:?}`, so a site formatting a
    /// differently-named binding slipped past) and brittle (funnelling both sites through one
    /// helper — the correct refactor — failed it). Redaction now lives in the only constructor.
    #[test]
    fn new_redacts_a_bearer_token_whatever_the_call_site_formats() {
        let e = SeamError::new("SyntaxError: 'ws://h/ws?token=supersecret' is invalid");
        assert!(!e.as_str().contains("supersecret"), "{}", e.as_str());
        assert!(e.as_str().contains("token=<redacted>"), "{}", e.as_str());

        // Idempotent, so a call site that also redacts is harmless.
        let twice = SeamError::new(e.as_str());
        assert_eq!(twice.as_str(), e.as_str());

        // Token-free text is untouched.
        assert_eq!(SeamError::new("socket closed").as_str(), "socket closed");

        // Multi-byte input cannot panic the byte-offset scan.
        let cjk = SeamError::new("接続失敗 token=秘密&x=1");
        assert!(!cjk.as_str().contains('秘'), "{}", cjk.as_str());
    }

    /// `Display` has no production caller yet, so pin it to `as_str` — a future
    /// `write!(f, "SeamError({})", ..)` would otherwise reshape any log line adopting it.
    #[test]
    fn display_matches_as_str() {
        let e = SeamError::for_test("boom");
        assert_eq!(e.to_string(), e.as_str());
    }
}
