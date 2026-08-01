//! The one place `cfg(feature)` splits web vs desktop. The reactivity spine sees only these
//! target-agnostic types; each target supplies the impl.
use std::path::PathBuf;

use otto_protocol::{Command, ServerMessage};

/// A failure diagnostic produced on the transport seam.
///
/// The i18n boundary (design spec `2026-07-31-ui-dioxus-i18n-design.md` §2) says these render
/// verbatim in every locale. That was a convention enforced by review; this type is the
/// enforcement. `new` is `pub(in crate::transport)`, so only this module and its per-target impls
/// can mint one — `net/`, `app.rs`, `components/`, and `desktop_boot.rs` can hold, compare, and
/// display a `SeamError`, but can never fabricate one out of crate-authored prose. That is what
/// makes `ClientText::Passthrough(SeamError)` a boundary rather than a comment.
///
/// The name means "this value reached the app through the transport seam", NOT "the transport
/// authored it": the workspace-RPC path (`web.rs`, `desktop.rs`) returns a server-sent
/// `WorkspaceResponse::Error` payload as a seam error. Both provenances are untranslated under
/// §2, so the distinction does not change how it renders.
///
/// Deliberately no `From<String>`/`From<&str>` and no `std::error::Error`: a blanket conversion
/// would be a public constructor by another name.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SeamError(String);

impl SeamError {
    /// Mint a diagnostic. Callable only from `transport/` and its per-target impls.
    pub(in crate::transport) fn new(detail: impl Into<String>) -> Self {
        Self(detail.into())
    }

    /// The diagnostic text, for rendering. Read-only by construction.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Mint one in a test that exercises a *consumer* of the seam rather than the seam itself
    /// (e.g. `net::view_model`'s row-rendering tests). `cfg(test)` so no production path reaches it.
    #[cfg(test)]
    pub fn for_test(detail: impl Into<String>) -> Self {
        Self(detail.into())
    }
}

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
    /// `SeamError`'s constructor must stay narrower than `pub(crate)`.
    ///
    /// This is the whole point of the type (spec §1): only `transport/` may mint one, so
    /// `ClientText::Passthrough` cannot be handed crate-authored prose from `net/`, `app.rs`,
    /// `components/`, or `desktop_boot.rs`. `pub(crate)` — the shape issue #120 originally
    /// sketched — is visible to every module in the crate and would silently restore exactly
    /// the freedom this type removes, with nothing else in the suite noticing.
    #[test]
    fn seam_error_has_no_crate_wide_constructor() {
        // Every needle scanned against the WHOLE file is assembled from fragments: this test
        // reads its own source, so a verbatim needle would match the line that spells it out —
        // the impl header would split on the test rather than on the type, and the two `From`
        // scans would fire on themselves and never be satisfiable.
        let impl_header = concat!("impl ", "SeamError {");
        let from_string = concat!("impl From<", "String> for SeamError");
        let from_str = concat!("impl From<", "&str> for SeamError");

        let src = include_str!("mod.rs");
        let block = src
            .split(impl_header)
            .nth(1)
            .expect("SeamError's inherent impl block");
        let block = block.split("\n}").next().expect("end of the impl block");
        assert!(
            block.contains("pub(in crate::transport) fn new("),
            "SeamError::new lost its transport-private visibility"
        );
        assert!(
            !block.contains("pub(crate)"),
            "a pub(crate) item in SeamError's impl re-opens the constructor to the whole crate"
        );
        // A blanket conversion is a public constructor by another name.
        assert!(
            !src.contains(from_string),
            "From<String> for SeamError is a public constructor by another name"
        );
        assert!(
            !src.contains(from_str),
            "From<&str> for SeamError is a public constructor by another name"
        );
    }

    /// The two `web.rs` sites that format a `JsValue` with `{e:?}` must keep `redact_token`.
    ///
    /// `ws_url` carries the bearer token as a query parameter (`build_ws_url`), and a rejected
    /// URL comes back as a `SyntaxError` that QUOTES THE URL IN FULL — so a `{e:?}` that skips
    /// `redact_token` ships the token into the visible event log, the surface most likely to be
    /// pasted into a bug report. Ten of the twelve `map_err` sites in `web.rs`/`desktop.rs` ARE
    /// a mechanical `e.to_string()` rewrite; these two are not, and the compiler cannot say so.
    ///
    /// A source scan rather than a behavioral test because `web.rs` is `cfg(feature = "web")`:
    /// its call sites can only be EXERCISED on wasm (which needs a webdriver and a version-matched
    /// `wasm-bindgen-test-runner`, and this repo has no CI), while `include_str!` sees the source
    /// under every feature combination — including the default `--features desktop` gate. The
    /// wasm test in `web_mount_test.rs` is the real guarantee; this is the one that runs by
    /// default.
    #[test]
    fn web_socket_error_paths_still_redact_the_bearer_token() {
        let src = include_str!("web.rs");
        let mut sites = 0;
        for (i, line) in src.lines().enumerate() {
            if line.contains("{e:?}") {
                sites += 1;
                assert!(
                    line.contains("redact_token"),
                    "web.rs:{}: a `{{e:?}}` diagnostic reaches the seam without redact_token: {}",
                    i + 1,
                    line.trim()
                );
            }
        }
        assert_eq!(
            sites, 2,
            "expected exactly the two JsValue error paths (WebSink::send, connect_impl) in web.rs"
        );
    }
}
