//! Wasm integration test for the **web mount → parse → connect** path.
//!
//! Why this file exists: the Dioxus runtime spike's single real runtime bug was a launch-params
//! parser (`net::url::parse_launch_params`) that had **no web call site**. The parser was
//! unit-tested in isolation and passed, so web autoconnect silently did nothing and shipped
//! anyway. No amount of parser-level assertion can catch a missing caller — the only test that
//! can is one that drives the actual mounted component and asserts the transport was reached.
//!
//! So these tests mount the real `App` in a real browser (`run_in_browser`), with a real
//! `location.search`, and assert on `transport::connect_probe` — the test-only recorder inside
//! `transport::connect`. Deleting the `#[cfg(feature = "web")]` autoconnect block from `app.rs`
//! makes `autoconnects_from_launch_params_on_mount` fail.
//!
//! Run with (see `.cargo/config.toml` for the one-off host tooling this needs):
//! ```text
//! cd ui-dioxus
//! CHROMEDRIVER=$(which chromedriver) cargo test --target wasm32-unknown-unknown --features web
//! ```
//!
//! **`--features web` is mandatory.** `cargo test --target wasm32-unknown-unknown` without it
//! compiles zero tests from this file and still reports success — the module is gated on
//! `all(test, feature = "web", target_arch = "wasm32")` (see `main.rs`).

use dioxus::core::NoOpMutations;
use dioxus::prelude::*;
use gloo_timers::future::TimeoutFuture;
use wasm_bindgen::JsValue;
use wasm_bindgen_test::{wasm_bindgen_test, wasm_bindgen_test_configure};

use crate::app::App;
use crate::transport::connect_probe;

// `web_sys::window()` (and therefore `location`/`history`) only exists in a browser, not in the
// default node runner.
wasm_bindgen_test_configure!(run_in_browser);

/// Bounded drive budget for the mounted VirtualDom: 200 × 5ms ≈ 1s. Bounded on purpose — a
/// regression must fail the test, never hang the browser runner. The negative-control tests
/// deliberately burn the whole budget: shortening it for them would let a merely *slow* connect
/// slip past and make "did not connect" pass vacuously.
const DRIVE_STEPS: usize = 200;
const DRIVE_STEP_MS: u32 = 5;

/// Extra steps to keep pumping *after* the first connect is observed, so a duplicate/looping
/// connect is observable rather than short-circuited away. Without this the "exactly once"
/// assertion is decorative: the loop would return at the first attempt and could never see a
/// second.
///
/// The regression it buys: `app.rs` documents the mount block's run-once property as load-bearing,
/// and `do_connect` performs tracked reads of `url`/`token` which the block then writes — so a
/// `use_future` → `use_effect` swap resubscribes into an unbounded reconnect loop. Measured with a
/// deliberately broken build, this turns 1 recorded connect into 41 and fails the assertion.
///
/// Note what actually holds the invariant today: the `history.replaceState` token scrub. Swapping
/// to `use_effect` *alone* does not loop, because the re-run reads an already-scrubbed (empty)
/// query and bails. Break both and the loop appears. The scrub is therefore load-bearing for
/// connect-once, not merely for keeping the bearer token out of the address bar.
const TAIL_STEPS: usize = 40;

/// Host/port used by the launch URLs below. Deliberately NOT the desktop wrapper's fixed 8787
/// sidecar port: `connect` records the URL and then dispatches to the real transport, which really
/// does construct a browser `WebSocket`. 65533 sits above Linux's default ephemeral range
/// (32768–60999), so nothing is expected to be listening and the test can't fire a bogus-token
/// connection at a developer's live `otto serve`. Do not "simplify" this back to 8080/8787.
const TEST_WS_BASE: &str = "ws://127.0.0.1:65533";
const TEST_TOKEN: &str = "probe-token-123";

/// The query exactly as the desktop wrapper writes it — `desktop/src-tauri/src/launch.rs`'s
/// `build_launch_url` emits `?ws={ws_base}&token={token}&autoconnect=1` with **no** percent
/// encoding. This is the producer half of the launch contract, so it is the form the primary test
/// feeds in: a test that only ever exercises a form its sole producer never emits is the same
/// producer/consumer wiring gap this file exists to close.
fn producer_query(autoconnect: &str) -> String {
    format!("ws={TEST_WS_BASE}&token={TEST_TOKEN}&autoconnect={autoconnect}")
}

/// The percent-encoded form. Not what the wrapper emits, but what a browser or a hand-built URL
/// may carry, and `parse_launch_params` decodes it — so it stays covered too.
fn percent_encoded_query(autoconnect: &str) -> String {
    format!(
        "ws={}&token={}&autoconnect={autoconnect}",
        urlencoding::encode(TEST_WS_BASE),
        urlencoding::encode(TEST_TOKEN),
    )
}

/// The `/ws` target `build_ws_url` must produce from those params on a fresh (no session, no
/// last_seq) connection.
fn expected_target() -> String {
    format!("{TEST_WS_BASE}/ws?token={TEST_TOKEN}")
}

/// Overwrite the page's query string in place (no navigation, no reload) so the mounted `App`
/// observes it through the ordinary `web_sys::window().location().search()` call it makes in
/// production. This is the browser-side half of the desktop wrapper's launch contract.
fn set_query(query: &str) {
    let win = web_sys::window().expect("test must run in a browser (run_in_browser)");
    let pathname = win.location().pathname().expect("location.pathname");
    let url = if query.is_empty() {
        pathname
    } else {
        format!("{pathname}?{query}")
    };
    win.history()
        .expect("history")
        .replace_state_with_url(&JsValue::NULL, "", Some(&url))
        .expect("replaceState");
}

fn current_search() -> String {
    web_sys::window()
        .expect("window")
        .location()
        .search()
        .expect("location.search")
}

/// One drive step: poll queued tasks, then flush the render.
///
/// The render is **not** optional. `VirtualDom::process_events` returns early while any scope is
/// dirty, and `poll_tasks` bails the moment a polled task dirties one — so without a render to
/// clear them, every step after the first signal write is a silent no-op and the loop wedges no
/// matter how large `DRIVE_STEPS` is. Rendering to `NoOpMutations` clears the dirty set without
/// needing a real DOM.
fn pump(dom: &mut VirtualDom) {
    dom.process_events();
    dom.render_immediate(&mut NoOpMutations);
}

/// Mount the real `App`, pump it, and return every URL the transport was asked to connect to.
///
/// `use_future` bodies are spawned during render and polled by `process_events`; the interleaved
/// timeout yields to the browser event loop so a mount path that gains an `.await` still gets
/// driven. Once a connect is observed, pumping continues for `TAIL_STEPS` more so duplicates show
/// up; if none is ever observed, the full `DRIVE_STEPS` budget is spent before giving up.
async fn mount_app_and_drive() -> Vec<String> {
    let mut dom = VirtualDom::new(App);
    dom.rebuild_in_place();

    let mut first_seen: Option<usize> = None;
    for step in 0..DRIVE_STEPS {
        pump(&mut dom);
        if first_seen.is_none() && !connect_probe::attempts().is_empty() {
            first_seen = Some(step);
        }
        if first_seen.is_some_and(|at| step >= at + TAIL_STEPS) {
            break;
        }
        TimeoutFuture::new(DRIVE_STEP_MS).await;
    }
    connect_probe::attempts()
}

/// THE regression test. A launch URL carrying `ws`/`token`/`autoconnect=1` — in the exact form the
/// desktop wrapper writes — must make the mounted app actually call `transport::connect`, with the
/// fully built `/ws?token=…` target, which proves the parsed params flowed all the way through
/// `build_ws_url` into the connect call rather than being parsed and dropped.
#[wasm_bindgen_test]
async fn autoconnects_from_launch_params_on_mount() {
    connect_probe::reset();
    set_query(&producer_query("1"));

    let attempts = mount_app_and_drive().await;

    assert_eq!(
        attempts,
        vec![expected_target()],
        "mounting App with an autoconnect launch URL must reach transport::connect exactly once \
         with the built ws target — an empty vec means the mount→connect call site is missing \
         (the exact bug this test exists to catch), not that the parser is wrong; more than one \
         means the mount block resubscribed into a reconnect loop"
    );

    // The mount path also scrubs the bearer token out of the visible URL once read. Asserting it
    // here confirms the block ran to completion, not just up to `do_connect`.
    assert_eq!(
        current_search(),
        "",
        "the mount path must scrub the token from the address bar via history.replaceState"
    );
}

/// The same contract via a percent-encoded query, which is what a browser or a hand-built URL may
/// actually carry. Guards the decode step in `parse_launch_params` at the integration level.
#[wasm_bindgen_test]
async fn autoconnects_from_percent_encoded_launch_params() {
    connect_probe::reset();
    set_query(&percent_encoded_query("1"));

    let attempts = mount_app_and_drive().await;

    assert_eq!(
        attempts,
        vec![expected_target()],
        "a percent-encoded launch query must decode to the same connect target"
    );
}

/// Negative control: proves the assertions above are actually sensitive to the launch params and
/// not passing because *any* mount connects. An ordinary browser visit (no query string) must
/// leave the manual connection form as the only way in.
#[wasm_bindgen_test]
async fn plain_visit_does_not_autoconnect() {
    connect_probe::reset();
    set_query("");

    let attempts = mount_app_and_drive().await;

    assert!(
        attempts.is_empty(),
        "a plain visit with no query string must not connect, got {attempts:?}"
    );
}

/// Negative control: `autoconnect` is the explicit opt-in. Params present but `autoconnect=0` must
/// not connect — this is the flag the desktop wrapper sets, and honoring it is what keeps a
/// hand-typed URL from triggering a connection.
#[wasm_bindgen_test]
async fn params_without_autoconnect_flag_do_not_connect() {
    connect_probe::reset();
    set_query(&producer_query("0"));

    let attempts = mount_app_and_drive().await;

    assert!(
        attempts.is_empty(),
        "autoconnect=0 must not connect, got {attempts:?}"
    );
}

/// The behavioral half of `transport::tests::web_socket_error_paths_still_redact_the_bearer_token`.
///
/// A structurally invalid URL (unclosed IPv6 literal) makes `WebSocket::new` reject with a
/// `SyntaxError` that quotes the URL IN FULL — including the `token=` query parameter — so this
/// asserts the actual string a user would see in the event log.
///
/// The URL must be malformed, not merely wrong-scheme: WHATWG normalizes `http`/`https` to
/// `ws`/`wss`, so `connect("http://…")` succeeds and yields no diagnostic at all.
///
/// Unlike the launch-param tests above, this one needs no unreachable port: `WebSocket::new`
/// rejects the URL during construction, so no socket is ever opened and there is nothing to keep
/// away from a developer's live `otto serve`. Do NOT "fix" the missing port by adding one — that
/// would turn a construction-time rejection into a real connection attempt.
#[wasm_bindgen_test]
fn connect_error_redacts_the_bearer_token() {
    let err = crate::transport::connect("ws://[::1/ws?token=supersecret")
        .err()
        .expect("a malformed URL must be rejected by WebSocket::new");
    assert!(
        !err.as_str().contains("supersecret"),
        "the bearer token leaked into a transport diagnostic: {}",
        err.as_str()
    );
    assert!(
        err.as_str().contains("token=<redacted>"),
        "expected the redaction marker, got: {}",
        err.as_str()
    );
}
