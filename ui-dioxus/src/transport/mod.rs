//! The one place `cfg(feature)` splits web vs desktop. The reactivity spine sees only these
//! target-agnostic types; each target supplies the impl.
use std::path::PathBuf;

use otto_protocol::{Command, ServerMessage};

/// A failure diagnostic that reached the app through the transport seam.
///
/// # What this type guarantees, precisely
///
/// **"Minted under `transport/`"** — a *location* claim, not a provenance one. `new` is callable
/// only from this module and its descendants, so `net/`, `app.rs`, `components/`, and
/// `desktop_boot.rs` can hold, compare, and render a `SeamError` but can never fabricate one.
/// That is what makes `ClientText::Passthrough(SeamError)` a boundary rather than a comment, and
/// it collapses the review surface for "is this text wrongly escaping localization?" from the
/// whole crate to three files.
///
/// It does **not** guarantee the text is server-authored. Two ways it is not:
/// - The workspace-RPC path returns a server-sent `WorkspaceResponse::Error` payload as a seam
///   error — server-authored, which is the direction you would expect.
/// - Four diagnostics in this subtree are crate-authored English (`"socket closed"`,
///   `"workspace rpc failed: HTTP {status}"`, `"unexpected response to List/Read"`, and the
///   no-feature fallback below). They render untranslated in every locale by design (i18n spec
///   §2), but the rule that keeps *new* interface copy out of `transport/` is still review, not
///   the compiler. Do not write user-facing instructions here.
///
/// # Why the redaction lives in the constructor
///
/// `build_ws_url` puts the bearer token in a query parameter, and a rejected URL comes back from
/// the browser quoting the URL in full. Redacting at each call site made "did this one remember?"
/// a per-site review question that a source-scanning test could only approximate — and it left
/// the desktop transport uncovered entirely. Because `new` is the single constructor, it is the
/// one place that makes the property structural: **no diagnostic can leave this seam carrying a
/// bearer token, whatever a future call site formats.** `redact_token` is idempotent, so double
/// redaction is harmless.
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
    /// The `pub(in crate::transport)` equals plain private here — a private item is already
    /// visible to its module's descendants, which is exactly `transport::{web,desktop}`. It is
    /// written explicitly because it states the intended boundary to a reader, and keeps its
    /// meaning if this type is ever moved up a level.
    pub(in crate::transport) fn new(detail: impl Into<String>) -> Self {
        Self(crate::net::url::redact_token(&detail.into()))
    }

    /// The diagnostic text, for rendering. Read-only by construction.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Mint one in a test that exercises a *consumer* of the seam rather than the seam itself
    /// (e.g. `net::view_model`'s row-rendering tests). `cfg(test)` so no production path reaches
    /// it — dropping that gate would hand every module a constructor, which
    /// `seam_error_has_no_crate_wide_constructor` asserts against.
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

/// An inbound socket event, delivered over the receiver `connect` returns.
pub enum SocketEvent {
    Message(Result<ServerMessage, SeamError>),
    Closed,
    Errored,
}

/// The outbound half of a live socket: serialize + send a `Command`. Boxed so the spine holds a
/// trait object, never a concrete socket type.
pub trait Sink {
    fn send(&self, cmd: &Command) -> Result<(), SeamError>;

    /// Tear the socket down explicitly. Dropping the sink is NOT enough on web — the inbound
    /// `web_sys` closures are `.forget()`'d, so the real `WebSocket` outlives the `Rc` and keeps
    /// delivering events. Every reconnect/disconnect path must call this on the old sink before
    /// installing a new one, so a stale socket can't push events into the new connection's state
    /// (mirrors `ui/src/app.rs`'s handler-detach + `close()` at the top of `connect`/`disconnect`).
    fn close(&self);
}

#[cfg(feature = "desktop")]
mod desktop;
#[cfg(feature = "web")]
mod web;

/// Test-only observation seam. `connect` records every target URL here before dispatching to the
/// real transport, so a test can assert that a *call site* — not merely a helper in isolation —
/// actually reached the transport. This exists because the runtime spike's one real bug was a
/// launch-params parser that was unit-tested and correct but had **no web call site**: the parser
/// passed, autoconnect silently did nothing, and it shipped. Asserting on parser output can never
/// catch that; asserting that `connect` was reached can.
///
/// Compiled only under `cfg(test)` on the wasm target — the shipped `web`/`desktop` builds
/// contain neither the recorder nor the `record` call (runtime behavior is unchanged), and a host
/// `cargo test` doesn't compile it either (its only consumer is the wasm-only `web_mount_test`, so
/// a bare `cfg(test)` gate would emit `dead_code` warnings on the default developer command).
#[cfg(all(test, target_arch = "wasm32"))]
pub mod connect_probe {
    use std::cell::RefCell;

    thread_local! {
        static ATTEMPTS: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    }

    pub(super) fn record(ws_url: &str) {
        ATTEMPTS.with(|a| a.borrow_mut().push(ws_url.to_string()));
    }

    /// Every URL passed to `connect` since the last `reset`, in call order.
    pub fn attempts() -> Vec<String> {
        ATTEMPTS.with(|a| a.borrow().clone())
    }

    /// Clear the record. Call at the start of each test: this recorder is process-global, and
    /// `wasm-bindgen-test` runs tests serialized (`CONCURRENCY = 1`), so each test starts from the
    /// previous one's leftovers rather than racing it.
    pub fn reset() {
        ATTEMPTS.with(|a| a.borrow_mut().clear());
    }
}

/// Open a socket to `ws_url`. Returns the outbound sink and a stream of inbound events.
pub fn connect(
    ws_url: &str,
) -> Result<
    (
        Box<dyn Sink>,
        futures_channel::mpsc::UnboundedReceiver<SocketEvent>,
    ),
    SeamError,
> {
    #[cfg(all(test, target_arch = "wasm32"))]
    connect_probe::record(ws_url);

    #[cfg(feature = "web")]
    {
        web::connect_impl(ws_url)
    }
    #[cfg(feature = "desktop")]
    {
        desktop::connect_impl(ws_url)
    }
    // Neither target feature enabled (e.g. `cargo test --no-default-features` for the pure
    // `net::` seam): the crate still needs to type-check, so fail closed at the call site
    // rather than fail to compile.
    #[cfg(not(any(feature = "web", feature = "desktop")))]
    {
        let _ = ws_url;
        Err(SeamError::new(
            "no transport feature enabled (build with --features web or --features desktop)",
        ))
    }
}

/// List every file in the served workspace (`POST /workspace` `List`).
pub async fn list_files(http_base: &str, token: &str) -> Result<Vec<PathBuf>, SeamError> {
    #[cfg(feature = "web")]
    {
        web::list_files_impl(http_base, token).await
    }
    #[cfg(feature = "desktop")]
    {
        desktop::list_files_impl(http_base, token).await
    }
    #[cfg(not(any(feature = "web", feature = "desktop")))]
    {
        let _ = (http_base, token);
        Err(SeamError::new(
            "no transport feature enabled (build with --features web or --features desktop)",
        ))
    }
}

/// Read one file's bytes (`POST /workspace` `Read`).
pub async fn read_file(http_base: &str, token: &str, path: PathBuf) -> Result<Vec<u8>, SeamError> {
    #[cfg(feature = "web")]
    {
        web::read_file_impl(http_base, token, path).await
    }
    #[cfg(feature = "desktop")]
    {
        desktop::read_file_impl(http_base, token, path).await
    }
    #[cfg(not(any(feature = "web", feature = "desktop")))]
    {
        let _ = (http_base, token, path);
        Err(SeamError::new(
            "no transport feature enabled (build with --features web or --features desktop)",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::SeamError;

    /// `SeamError` must expose exactly one production constructor, narrower than `pub(crate)`.
    ///
    /// The compiler is the real enforcement; this test catches a later *widening*. An earlier
    /// version asserted `!block.contains("pub(crate)")` — a blocklist, which review showed
    /// green-lit every actual bypass: a `pub` tuple field, `derive(Default)`, an extra `pub fn`,
    /// a `FromStr`, and a `From` impl in a sibling module the scan never read. It is now a
    /// whitelist over four independently-bypassable surfaces, and it scans the siblings too.
    ///
    /// Every needle checked against a whole file is assembled with `concat!`: this test reads its
    /// own source, so a verbatim literal would match the line that spells it out.
    #[test]
    fn seam_error_has_no_crate_wide_constructor() {
        let struct_decl = concat!("pub struct ", "SeamError(String);");
        let impl_header = concat!("impl ", "SeamError {");
        let trait_impl_marker = concat!("for ", "SeamError");

        let mod_rs = include_str!("mod.rs");
        let siblings = [include_str!("web.rs"), include_str!("desktop.rs")];

        // 1. A `pub` tuple field is a total bypass, and lives outside the impl block.
        assert!(
            mod_rs.contains(struct_decl),
            "SeamError's tuple field is no longer private"
        );

        // 2. `derive(Default)` synthesizes a public constructor.
        let derives = mod_rs
            .lines()
            .find(|l| l.trim_start().starts_with("#[derive(") && l.contains("Clone"))
            .expect("SeamError's derive line");
        assert!(
            !derives.contains("Default"),
            "derive(Default) synthesizes a public SeamError constructor"
        );

        // 3. Whitelist the impl block's fns, so a new `pub fn mint(..)` fails.
        let block = mod_rs
            .split(impl_header)
            .nth(1)
            .expect("SeamError's inherent impl block");
        let block = block.split("\n}").next().expect("end of the impl block");
        let fns: Vec<&str> = block
            .lines()
            .map(str::trim)
            .filter(|l| l.contains(" fn "))
            .collect();
        assert_eq!(
            fns,
            [
                "pub(in crate::transport) fn new(detail: impl Into<String>) -> Self {",
                "pub fn as_str(&self) -> &str {",
                "pub fn for_test(detail: impl Into<String>) -> Self {",
            ],
            "SeamError's constructor set changed — a new one must be justified here"
        );

        // 4. Ungating `for_test` alone makes it a crate-wide public constructor.
        assert!(
            block.contains(concat!("#[cfg(", "test)]\n    pub fn for_test")),
            "SeamError::for_test lost its cfg(test) gate — it is now a public constructor"
        );

        // 5. No trait impl under `transport/` may launder a value in. `Display` is the one
        //    allowed impl; `From`/`FromStr`/`Deref` are constructors by another name, and a
        //    sibling module is exactly where the previous version of this test was blind.
        for src in std::iter::once(mod_rs).chain(siblings) {
            for line in src.lines() {
                let t = line.trim();
                if t.starts_with("impl") && t.contains(trait_impl_marker) {
                    assert!(
                        t.contains("std::fmt::Display"),
                        "disallowed trait impl for SeamError: {t}"
                    );
                }
            }
        }
    }

    /// `new` redacts, so every diagnostic leaving the seam is safe by construction.
    ///
    /// This replaces a source scan asserting the two `web.rs` sites called `redact_token` by
    /// hand. That scan was evadable (it keyed on the literal `{e:?}`, so a site formatting a
    /// differently-named binding slipped past) and brittle (funnelling both sites through one
    /// helper — the correct refactor — failed it). Redaction now lives in the single
    /// constructor: a property rather than a formatting snapshot, and it covers `desktop.rs`.
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
    }

    /// `Display` has no production caller yet, so pin it to `as_str` — a future
    /// `write!(f, "SeamError({})", ..)` would otherwise reshape any log line adopting it.
    #[test]
    fn display_matches_as_str() {
        let e = SeamError::for_test("boom");
        assert_eq!(e.to_string(), e.as_str());
    }
}
