//! Wasm integration test for the **web mount → parse → connect** path.
//!
//! Why this file exists: the Dioxus runtime spike's single real runtime bug was a launch-params
//! parser (`net::url::parse_launch_params`) that had **no web call site**. The parser was
//! unit-tested in isolation and passed, so web autoconnect silently did nothing and shipped
//! anyway. No amount of parser-level assertion can catch a missing caller — the only test that
//! can is one that drives the actual mounted component and asserts the transport was reached.
//!
//! So these tests mount the real `App` in a real browser (`run_in_browser`), with a real
//! `location.search`, and assert on `transport::connect_probe` — the `cfg(test)`-only recorder
//! inside `transport::connect`. Deleting the `#[cfg(feature = "web")]` autoconnect block from
//! `app.rs` makes `autoconnects_from_launch_params_on_mount` fail.
//!
//! Run with (see `.cargo/config.toml` for the one-off host tooling this needs):
//! ```text
//! cd ui-dioxus
//! CHROMEDRIVER=$(which chromedriver) cargo test --target wasm32-unknown-unknown --features web
//! ```

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

/// Host/port used by the launch URLs below. Deliberately NOT the desktop wrapper's fixed 8787
/// sidecar port: `connect` records the URL and then dispatches to the real transport, which
/// really does construct a browser `WebSocket`. Pointing at a port nothing listens on keeps the
/// test from firing a bogus-token connection at a developer's live `otto serve`.
const TEST_WS_BASE: &str = "ws://127.0.0.1:65533";
const TEST_TOKEN: &str = "probe-token-123";

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

/// The desktop wrapper's launch query, with `autoconnect` set to `flag` ("1" opts in).
fn launch_query(flag: &str) -> String {
    format!(
        "ws={}&token={}&autoconnect={flag}",
        urlencoding::encode(TEST_WS_BASE),
        urlencoding::encode(TEST_TOKEN),
    )
}

fn current_search() -> String {
    web_sys::window()
        .expect("window")
        .location()
        .search()
        .expect("location.search")
}

/// Mount the real `App` and pump the VirtualDom until it reaches the transport (or the budget
/// runs out). `use_future` bodies are spawned during render and polled by `process_events`; the
/// interleaved timeout yields to the browser event loop so an `await` added to the mount path
/// later still gets driven rather than silently never running.
async fn mount_app_and_drive() {
    let mut dom = VirtualDom::new(App);
    dom.rebuild_in_place();
    for _ in 0..DRIVE_STEPS {
        dom.process_events();
        if !connect_probe::attempts().is_empty() {
            return;
        }
        TimeoutFuture::new(DRIVE_STEP_MS).await;
    }
}

/// THE regression test. A launch URL carrying `ws`/`token`/`autoconnect=1` must make the mounted
/// app actually call `transport::connect` — and with the fully built `/ws?token=…` target, which
/// proves the parsed params flowed all the way through `build_ws_url` into the connect call
/// rather than being parsed and dropped.
#[wasm_bindgen_test]
async fn autoconnects_from_launch_params_on_mount() {
    connect_probe::reset();
    set_query(&launch_query("1"));

    mount_app_and_drive().await;

    assert_eq!(
        connect_probe::attempts(),
        vec![format!("{TEST_WS_BASE}/ws?token={TEST_TOKEN}")],
        "mounting App with an autoconnect launch URL must reach transport::connect exactly once \
         with the built ws target — an empty vec means the mount→connect call site is missing \
         (the exact bug this test exists to catch), not that the parser is wrong"
    );

    // The mount path also scrubs the bearer token out of the visible URL once read. Asserting it
    // here confirms the block ran to completion, not just up to `do_connect`.
    assert_eq!(
        current_search(),
        "",
        "the mount path must scrub the token from the address bar via history.replaceState"
    );
}

/// Negative control: proves the assertion above is actually sensitive to the launch params and
/// not passing because *any* mount connects. An ordinary browser visit (no query string) must
/// leave the manual connection form as the only way in.
#[wasm_bindgen_test]
async fn plain_visit_does_not_autoconnect() {
    connect_probe::reset();
    set_query("");

    mount_app_and_drive().await;

    assert!(
        connect_probe::attempts().is_empty(),
        "a plain visit with no query string must not connect, got {:?}",
        connect_probe::attempts()
    );
}

/// Negative control: `autoconnect` is the explicit opt-in. Params present but `autoconnect=0`
/// must not connect — this is the flag the desktop wrapper sets, and honoring it is what keeps a
/// hand-typed URL from triggering a connection.
#[wasm_bindgen_test]
async fn params_without_autoconnect_flag_do_not_connect() {
    connect_probe::reset();
    set_query(&launch_query("0"));

    mount_app_and_drive().await;

    assert!(
        connect_probe::attempts().is_empty(),
        "autoconnect=0 must not connect, got {:?}",
        connect_probe::attempts()
    );
}
