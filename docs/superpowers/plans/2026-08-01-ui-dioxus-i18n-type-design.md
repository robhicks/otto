# ui-dioxus i18n Type-Design Follow-ups Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Convert two `ui-dioxus` i18n conventions into compiler-enforced invariants — the translate-vs-passthrough boundary becomes a type only `transport/` can mint, and the event log's CSS class becomes a derived function instead of a second field that can contradict the first — and settle the one boundary judgment the i18n spec left open by localizing the sidecar-spawn failure's framing while passing its payload through verbatim.

**Architecture:** A newtype `SeamError` in `transport/mod.rs` with a `pub(in crate::transport)` constructor replaces `String` as the transport seam's error type, so `ClientText::Passthrough(SeamError)` is unfabricatable outside `transport/`. `LogRow` is deleted in favour of `RowMsg::class()`. `ClientText::Authored` grows an args vector so a parameterized catalog key (`Msg::SidecarSpawnFailed`) can frame an untranslatable payload — the shape `Msg::RowServerError` already establishes.

**Tech Stack:** Rust (edition 2021 in this crate; workspace toolchain pinned 1.85), Dioxus 0.7. No new dependencies — the bundle budget in `ui-dioxus/scripts/build-web.sh` is unaffected.

**Spec:** `docs/superpowers/specs/2026-08-01-ui-dioxus-i18n-type-design.md` — read it first. This plan implements it exactly.

## Global Constraints

- **`ui-dioxus/` stays workspace-excluded.** `cargo build --workspace` / `cargo test --workspace` must be byte-for-byte unaffected. **No file under `crates/` is touched.** The only files outside `ui-dioxus/` that change are `docs/superpowers/specs/2026-07-31-ui-dioxus-i18n-design.md` and the repo-root `CLAUDE.md`, both in Task 4.
- **No new dependencies.** In particular no `trybuild` (spec Assumption 6). `scripts/build-web.sh`'s `MAX_WASM_BYTES` guard must stay satisfied; all new tests are `#[cfg(test)]` so nothing enters the release wasm.
- **Three build configurations must keep compiling:** `--features desktop`, `--target wasm32-unknown-unknown --features web`, and `--no-default-features` (the pure-seam check — `transport/mod.rs`'s three `#[cfg(not(any(feature = "web", feature = "desktop")))]` fallback arms exist for it).
- **The redaction sites are NOT a mechanical rewrite.** `web.rs:27` and `web.rs:50` are the only two of the twelve `map_err` sites that are not a plain `e.to_string()`; both are `.map_err(|e| redact_token(&format!("{e:?}")))` and both **must keep `redact_token`**. Dropping it ships the bearer token into the visible event log. See Task 1.
- **`SeamError` gets no `From<String>` / `From<&str>` impl and no `std::error::Error` impl.** A blanket conversion is a public constructor by another name (spec Assumptions 4 + §1).
- **The catalog's exhaustive matches never gain a wildcard arm.** `t`'s `(Locale, Msg)` match and the new `RowMsg::class()` match are both exhaustive-with-no-`_` on purpose: that is what makes a new variant a compile error rather than a silent gap.
- **`style.css` is not touched.** `row-approval` and `row-meter` have no stylesheet rule today (`style.css:41-46` covers the other six); that is pre-existing, cosmetic, and out of scope.
- **No Claude/AI self-attribution** in any commit message, comment, doc, or PR body.
- Run `cargo fmt --all` before every Rust commit (rustfmt is pinned in `rust-toolchain.toml`). Run it **from inside `ui-dioxus/`** — that crate is its own workspace, so a repo-root `cargo fmt --all` does not reach it.
- Default test command for every task: `cd ui-dioxus && cargo test --features desktop` (176 tests pass on `main` today).
- Wasm compile check for every task that touches a `cfg`-gated path: `cd ui-dioxus && cargo build --target wasm32-unknown-unknown --features web`.

## File Structure

| File | Responsibility |
|---|---|
| `ui-dioxus/src/transport/mod.rs` | **Modify.** Add `SeamError` (+ private ctor, `as_str`, `Display`, `cfg(test)` `for_test`); change `SocketEvent`, `Sink::send`, `connect`, `list_files`, `read_file` to carry it; three fallback arms; new `#[cfg(test)] mod tests` with the two source-scan guards. |
| `ui-dioxus/src/transport/web.rs` | **Modify.** Eleven error-construction sites become `SeamError`; the two `redact_token` sites keep it. |
| `ui-dioxus/src/transport/desktop.rs` | **Modify.** Ten error-construction sites become `SeamError`. |
| `ui-dioxus/src/net/view_model.rs` | **Modify.** `ClientText::Passthrough(SeamError)`; `ClientText::Authored { msg, args }` + constructors; `RowMsg::class()`; delete `LogRow` + `row()`; `describe_event`/`error_row`/`client_error_row` return `RowMsg`; `render_row`'s `ClientError` arm. |
| `ui-dioxus/src/components/event_log.rs` | **Modify.** `Signal<Vec<RowMsg>>`; `class: "row {r.class()}"`; `render_row(locale, r)`. |
| `ui-dioxus/src/app.rs` | **Modify.** `rows` signal type; `ClientText::authored(..)` at `:94`; the `SpawnFailed` arm at `:454`. |
| `ui-dioxus/src/desktop_boot.rs` | **Modify.** `BootOutcome::SpawnFailed { bin, detail }`; `boot()` stops pre-formatting the UI sentence. |
| `ui-dioxus/src/i18n/catalog.rs` | **Modify.** Add `SidecarSpawnFailed` (5 locales); extend `protocol_identifiers_survive_translation`. |
| `ui-dioxus/src/web_mount_test.rs` | **Modify.** Add the wasm behavioral redaction test. |
| `docs/superpowers/specs/2026-07-31-ui-dioxus-i18n-design.md` | **Modify.** §2 amendment recording the §3 decision. |
| `CLAUDE.md` | **Modify.** Narrow the boundary sentence at `:55-56` from "transport/boot diagnostics" to transport diagnostics only. |

## Task Order & Rationale

**Task 1 first** because it has the widest blast radius: changing the transport seam's error type touches three transport files plus every `ClientText::Passthrough` call site, and every later task builds on a compiling seam. It also carries the one security-relevant edit (the two redaction sites), so it gets its own reviewer gate rather than being buried in a larger diff.

**Task 2** (delete `LogRow`) is independent of Task 1 in principle but sequenced after it because both touch `net/view_model.rs` and `app.rs`; doing them in one file at a time avoids an implementer resolving a self-inflicted conflict.

**Task 3** (the `SidecarSpawnFailed` decision) depends on Task 1: it changes `ClientText::Authored`'s shape, and Task 1 has already established that `Passthrough` carries a `SeamError`, which is what makes "`desktop_boot` can no longer produce a `Passthrough`" true rather than merely conventional.

**Task 4** is docs only, and runs last so it records what actually shipped.

---

### Task 1: `SeamError` — the transport seam carries a typed diagnostic

**Files:**
- Modify: `ui-dioxus/src/transport/mod.rs`
- Modify: `ui-dioxus/src/transport/web.rs`
- Modify: `ui-dioxus/src/transport/desktop.rs`
- Modify: `ui-dioxus/src/net/view_model.rs:19-22` (the `ClientText` enum), `:114-120` (`render_row`'s `ClientError` arm), `:582` (the existing test)
- Modify: `ui-dioxus/src/web_mount_test.rs` (append one test)
- Test: same files (`#[cfg(test)] mod tests`, per repo convention — tests live next to code)

**Interfaces:**
- Produces: `crate::transport::SeamError` with `pub fn as_str(&self) -> &str`, `pub(in crate::transport) fn new(detail: impl Into<String>) -> Self`, `#[cfg(test)] pub fn for_test(detail: impl Into<String>) -> Self`, and `impl Display`. Derives `Clone, PartialEq, Eq, Debug`.
- Produces: `SocketEvent::Message(Result<ServerMessage, SeamError>)`; `Sink::send(&self, cmd: &Command) -> Result<(), SeamError>`; `connect(..) -> Result<(Box<dyn Sink>, UnboundedReceiver<SocketEvent>), SeamError>`; `list_files(..) -> Result<Vec<PathBuf>, SeamError>`; `read_file(..) -> Result<Vec<u8>, SeamError>`.
- Produces: `ClientText::Passthrough(SeamError)` (the `Authored` arm is unchanged in this task — Task 3 changes it).
- Consumes: nothing from earlier tasks.

- [ ] **Step 1: Write the failing tests**

Append a `#[cfg(test)] mod tests` block at the END of `ui-dioxus/src/transport/mod.rs`:

```rust
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
        let src = include_str!("mod.rs");
        let block = src
            .split("impl SeamError {")
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
            !src.contains("impl From<String> for SeamError"),
            "From<String> for SeamError is a public constructor by another name"
        );
        assert!(
            !src.contains("impl From<&str> for SeamError"),
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
    /// wasm behavioral test in `web_mount_test.rs` is the real guarantee; this is the one that
    /// runs by default.
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
```

Append this test to `ui-dioxus/src/web_mount_test.rs` (match the file's existing attribute style — it is gated `#[cfg(all(test, feature = "web", target_arch = "wasm32"))]` at the module level, so a bare `#[wasm_bindgen_test]` is correct here):

```rust
/// The behavioral half of `transport::tests::web_socket_error_paths_still_redact_the_bearer_token`.
///
/// A scheme-invalid URL makes `WebSocket::new` reject with a `SyntaxError` that quotes the URL —
/// including the `token=` query parameter — so this asserts the real string a user would see.
#[wasm_bindgen_test]
fn connect_error_redacts_the_bearer_token() {
    let err = crate::transport::connect("http://127.0.0.1:8787/ws?token=supersecret")
        .err()
        .expect("a non-ws scheme must be rejected by WebSocket::new");
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
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cd ui-dioxus && cargo test --features desktop transport::`
Expected: FAIL to COMPILE — `include_str!("mod.rs")`'s `split("impl SeamError {")` returns `None` because `SeamError` does not exist yet, so `seam_error_has_no_crate_wide_constructor` panics on `.expect("SeamError's inherent impl block")`. `web_socket_error_paths_still_redact_the_bearer_token` PASSES already (the two sites exist and redact today) — that is correct and expected: it is a *regression* guard, and it must be green before the change so a red after the change is unambiguous. Record both outcomes.

Run: `cd ui-dioxus && cargo build --target wasm32-unknown-unknown --features web --tests`
Expected: FAIL — `err.as_str()` does not exist on `String`.

- [ ] **Step 3: Add `SeamError` to `transport/mod.rs`**

Insert directly above the `SocketEvent` enum (keep the existing module doc comment at the top of the file):

```rust
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
```

Then change the four seam signatures in the same file:

- `SocketEvent::Message(Result<ServerMessage, String>)` → `SocketEvent::Message(Result<ServerMessage, SeamError>)`
- `fn send(&self, cmd: &Command) -> Result<(), String>` → `-> Result<(), SeamError>`
- `connect`'s return type `..., String>` → `..., SeamError>`
- `list_files` → `Result<Vec<PathBuf>, SeamError>`; `read_file` → `Result<Vec<u8>, SeamError>`

And the three `#[cfg(not(any(feature = "web", feature = "desktop")))]` fallback arms, each currently `Err("no transport feature enabled (build with --features web or --features desktop)".to_string())`:

```rust
        Err(SeamError::new(
            "no transport feature enabled (build with --features web or --features desktop)",
        ))
```

- [ ] **Step 4: Convert `transport/web.rs`'s eleven error sites**

Add `use super::{Sink, SeamError, SocketEvent};` (extend the existing `use super::{Sink, SocketEvent};` at `:15`). Then, by current line number:

| Line | Before | After |
|---|---|---|
| 21 | `serde_json::to_string(cmd).map_err(\|e\| e.to_string())?` | `.map_err(\|e\| SeamError::new(e.to_string()))?` |
| 27 | `.map_err(\|e\| redact_token(&format!("{e:?}")))` | `.map_err(\|e\| SeamError::new(redact_token(&format!("{e:?}"))))` |
| 50 | `WebSocket::new(ws_url).map_err(\|e\| redact_token(&format!("{e:?}")))?` | `WebSocket::new(ws_url).map_err(\|e\| SeamError::new(redact_token(&format!("{e:?}"))))?` |
| 55 | `.map_err(\|err\| err.to_string())` | `.map_err(\|err\| SeamError::new(err.to_string()))` |
| 88 | `.map_err(\|e\| e.to_string())?` | `.map_err(\|e\| SeamError::new(e.to_string()))?` |
| 91 | `.map_err(\|e\| e.to_string())?` | `.map_err(\|e\| SeamError::new(e.to_string()))?` |
| 93 | `return Err(format!("workspace rpc failed: HTTP {}", resp.status()));` | `return Err(SeamError::new(format!("workspace rpc failed: HTTP {}", resp.status())));` |
| 95 | `.map_err(\|e\| e.to_string())?` | `.map_err(\|e\| SeamError::new(e.to_string()))?` |
| 97 | `return Err(message.clone());` | `return Err(SeamError::new(message.clone()));` |
| 113 | `other => Err(format!("unexpected response to List: {other:?}")),` | `other => Err(SeamError::new(format!("unexpected response to List: {other:?}"))),` |
| 124 | `other => Err(format!("unexpected response to Read: {other:?}")),` | `other => Err(SeamError::new(format!("unexpected response to Read: {other:?}"))),` |

Change `rpc`'s return type to `Result<WorkspaceResponse, SeamError>` and `connect_impl`/`list_files_impl`/`read_file_impl`'s to match `mod.rs`.

**Lines 27 and 50 keep `redact_token`.** Do not "simplify" them to `e.to_string()`.

**Do not write `{e:?}` in any new comment in `web.rs`** — `web_socket_error_paths_still_redact_the_bearer_token` scans every line containing it and requires `redact_token` on the same line.

- [ ] **Step 5: Convert `transport/desktop.rs`'s ten error sites**

Extend the `use super::{...}` import with `SeamError`, then:

| Line | Before | After |
|---|---|---|
| 20 | `serde_json::to_string(cmd).map_err(\|e\| e.to_string())?` | `.map_err(\|e\| SeamError::new(e.to_string()))?` |
| 22 | `Some(tx) => tx.send(json).map_err(\|e\| e.to_string()),` | `Some(tx) => tx.send(json).map_err(\|e\| SeamError::new(e.to_string())),` |
| 23 | `None => Err("socket closed".to_string()),` | `None => Err(SeamError::new("socket closed")),` |
| 63 | `serde_json::from_str::<ServerMessage>(&txt).map_err(\|e\| e.to_string());` | `.map_err(\|e\| SeamError::new(e.to_string()));` |
| 102 | `.map_err(\|e\| e.to_string())?;` | `.map_err(\|e\| SeamError::new(e.to_string()))?;` |
| 104 | `return Err(format!("workspace rpc failed: HTTP {}", resp.status()));` | `return Err(SeamError::new(format!("workspace rpc failed: HTTP {}", resp.status())));` |
| 106 | `.map_err(\|e\| e.to_string())?;` | `.map_err(\|e\| SeamError::new(e.to_string()))?;` |
| 108 | `return Err(message.clone());` | `return Err(SeamError::new(message.clone()));` |
| 124 | `other => Err(format!("unexpected response to List: {other:?}")),` | `other => Err(SeamError::new(format!("unexpected response to List: {other:?}"))),` |
| 135 | `other => Err(format!("unexpected response to Read: {other:?}")),` | `other => Err(SeamError::new(format!("unexpected response to Read: {other:?}"))),` |

Change `connect_impl`/`rpc`/`list_files_impl`/`read_file_impl`'s return types to match `mod.rs`.

- [ ] **Step 6: Point `ClientText::Passthrough` at `SeamError`**

In `ui-dioxus/src/net/view_model.rs`, add `use crate::transport::SeamError;` to the imports, then:

```rust
pub enum ClientText {
    Authored(Msg),
    Passthrough(SeamError),
}
```

Update the doc comment at `:38-40` — it currently says "only `Passthrough(String)` is verbatim":

```rust
/// - `ClientError(ClientText)` carries both kinds: the `Authored(Msg)` arm IS translated (it is
///   this crate's own copy, e.g. `Msg::UrlAndTokenRequired`); only `Passthrough(SeamError)` is
///   verbatim — and `SeamError`'s constructor is private to `transport/`, so that arm can only
///   ever carry a value the transport seam produced.
```

And `render_row`'s `Passthrough` arm at `:117`:

```rust
                ClientText::Passthrough(e) => e.as_str().to_string(),
```

Update the existing test at `:582`:

```rust
        let passthrough = client_error_row(ClientText::Passthrough(SeamError::for_test(
            "socket closed",
        )));
```

`app.rs` needs **no change in this task** — its six `Passthrough` sites forward whatever the seam handed them and never name the type. Verify that by compiling, not by editing.

- [ ] **Step 7: Run the host tests**

Run: `cd ui-dioxus && cargo test --features desktop`
Expected: PASS — 178 tests (176 baseline + the 2 new `transport::tests`).

Run: `cd ui-dioxus && cargo clippy --features desktop --all-targets`
Expected: no new warnings.

- [ ] **Step 8: Run the wasm build and the wasm tests**

Run: `cd ui-dioxus && cargo build --target wasm32-unknown-unknown --features web`
Expected: PASS.

Run: `cd ui-dioxus && CHROMEDRIVER=$(which chromedriver) cargo test --target wasm32-unknown-unknown --features web`
Expected: PASS — 5 tests (4 baseline + `connect_error_redacts_the_bearer_token`).

If `chromedriver` or a version-matched `wasm-bindgen-test-runner` is missing, report it in the task report rather than deleting the test — the host source-scan still covers the regression, and the wasm test is expected to be harness-only.

- [ ] **Step 9: Prove the boundary actually holds (manual, one command, do not commit the result)**

Temporarily add `let _ = crate::transport::SeamError::new("prose");` to the top of `describe_event` in `net/view_model.rs` and run `cd ui-dioxus && cargo build --features desktop`.
Expected: `error[E0624]: method \`new\` is private`. Then **revert that line**. This confirms the invariant is the compiler's, not the test's. Note the observed error code in the task report.

- [ ] **Step 10: Format and commit**

```bash
cd ui-dioxus && cargo fmt --all && cd ..
git add ui-dioxus/src/transport/ ui-dioxus/src/net/view_model.rs ui-dioxus/src/web_mount_test.rs
git commit -m "ui-dioxus: make the transport seam carry a typed SeamError"
```

---

### Task 2: delete `LogRow`; derive the row class from `RowMsg`

**Files:**
- Modify: `ui-dioxus/src/net/view_model.rs:57-67` (delete `LogRow` + `row()`), `:329-399` (`describe_event`/`error_row`/`client_error_row`), and the tests at `:445`, `:736`, `:762`
- Modify: `ui-dioxus/src/components/event_log.rs`
- Modify: `ui-dioxus/src/app.rs:16-19` (imports), `:51` (the signal)
- Test: `ui-dioxus/src/net/view_model.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `ClientText::Passthrough(SeamError)` from Task 1.
- Produces: `impl RowMsg { pub fn class(&self) -> &'static str }`; `describe_event(kind: &EventKind) -> RowMsg`; `error_row(message: &str) -> RowMsg`; `client_error_row(text: ClientText) -> RowMsg`. `LogRow` and the private `row()` no longer exist.

- [ ] **Step 1: Write the failing test**

Add to `net/view_model.rs`'s `mod tests`:

```rust
    #[test]
    fn row_classes_are_pinned_per_variant() {
        // `class` used to be a second FIELD alongside `msg`, which made
        // `LogRow { class: "row-agent", msg: RowMsg::ServerError { .. } }` representable. It is a
        // total 1:1 function of the variant, so it is now derived and that state is unrepresentable.
        //
        // Deliberately pinned against hardcoded expectations rather than checked against
        // `style.css`: `style.css:41-46` defines only `.row-agent`/`.row-edit`/`.row-verify`/
        // `.row-log`/`.row-turn`/`.row-error`, so `row-approval` and `row-meter` have no rule and
        // inherit `.row`. That gap is pre-existing and out of scope; a stylesheet-derived test
        // would fail for a reason this change did not cause.
        let cases: [(RowMsg, &str); 10] = [
            (RowMsg::AgentStarted { role: "Planner".into() }, "row-agent"),
            (RowMsg::AgentFinished { role: "Planner".into() }, "row-agent"),
            (RowMsg::FileEdit { path: "a.rs".into(), bytes: 1 }, "row-edit"),
            (RowMsg::Verify { ok: true, detail: String::new() }, "row-verify"),
            (RowMsg::Log { message: "hi".into() }, "row-log"),
            (RowMsg::TurnComplete { ok: true }, "row-turn"),
            (RowMsg::ApprovalRequest { path: "a.rs".into() }, "row-approval"),
            (RowMsg::Meter { input: 1, output: 2 }, "row-meter"),
            (RowMsg::ServerError { message: "boom".into() }, "row-error"),
            (
                RowMsg::ClientError(ClientText::Authored(Msg::UrlAndTokenRequired)),
                "row-error",
            ),
        ];
        for (msg, expected) in &cases {
            assert_eq!(msg.class(), *expected, "{msg:?}");
        }
    }
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cd ui-dioxus && cargo test --features desktop row_classes_are_pinned`
Expected: FAIL to compile — `no method named \`class\` found for reference \`&RowMsg\``.

- [ ] **Step 3: Add `RowMsg::class()` and delete `LogRow`**

Add directly below the `RowMsg` enum in `net/view_model.rs`:

```rust
impl RowMsg {
    /// The row's CSS class.
    ///
    /// A TOTAL function of the variant — which is precisely why there is no struct pairing the
    /// two. `LogRow { class, msg }` made a class that contradicts its content representable
    /// (`class: "row-agent"` beside `RowMsg::ServerError`); deriving it makes that unrepresentable.
    ///
    /// Exhaustive with no wildcard arm, for the same reason `t`'s `(Locale, Msg)` match has none:
    /// a new `RowMsg` variant must be a compile error here, not a row that silently renders
    /// unclassed.
    pub fn class(&self) -> &'static str {
        match self {
            RowMsg::AgentStarted { .. } | RowMsg::AgentFinished { .. } => "row-agent",
            RowMsg::FileEdit { .. } => "row-edit",
            RowMsg::Verify { .. } => "row-verify",
            RowMsg::Log { .. } => "row-log",
            RowMsg::TurnComplete { .. } => "row-turn",
            RowMsg::ApprovalRequest { .. } => "row-approval",
            RowMsg::Meter { .. } => "row-meter",
            RowMsg::ServerError { .. } | RowMsg::ClientError(_) => "row-error",
        }
    }
}
```

Delete the `LogRow` struct (`:57-63`) and the `row()` helper (`:65-67`) entirely.

Rewrite the three constructors to return `RowMsg` — each loses its `row("class", ...)` wrapper:

```rust
/// Classify one engine event into a structured log row. Formatting happens later, in `render_row`;
/// the CSS class is derived on demand by `RowMsg::class()`.
pub fn describe_event(kind: &EventKind) -> RowMsg {
    match kind {
        EventKind::AgentStarted { role } => RowMsg::AgentStarted {
            role: format!("{role:?}"),
        },
        EventKind::AgentFinished { role } => RowMsg::AgentFinished {
            role: format!("{role:?}"),
        },
        EventKind::FileEdit {
            path,
            bytes_written,
        } => RowMsg::FileEdit {
            path: path.display().to_string(),
            bytes: *bytes_written,
        },
        EventKind::VerifyResult { ok, detail } => RowMsg::Verify {
            ok: *ok,
            detail: detail.clone(),
        },
        EventKind::Log { message } => RowMsg::Log {
            message: message.clone(),
        },
        EventKind::TurnComplete { ok } => RowMsg::TurnComplete { ok: *ok },
        EventKind::ApprovalRequest { path, .. } => RowMsg::ApprovalRequest {
            path: path.display().to_string(),
        },
        EventKind::TokenCostMeter {
            input_tokens,
            output_tokens,
        } => RowMsg::Meter {
            input: *input_tokens,
            output: *output_tokens,
        },
    }
}

/// A server-sent `Error` frame as a row. The message is engine-originated and passes through.
pub fn error_row(message: &str) -> RowMsg {
    RowMsg::ServerError {
        message: message.to_string(),
    }
}

/// A client-side problem as a row — authored copy or a passthrough diagnostic (i18n spec §2).
pub fn client_error_row(text: ClientText) -> RowMsg {
    RowMsg::ClientError(text)
}
```

Update the doc comment on `RowMsg` (`:24-42`) — its first line still says "A rendered row's content"; leave the body, but add one sentence noting the class is derived by `class()`.

- [ ] **Step 4: Update the three existing tests that read `.class`**

`:445` → `assert_eq!(r.class(), "row-edit");`
`:736` → `assert_eq!(r.class(), "row-approval");`
`:762` → `assert_eq!(r.class(), "row-meter");`

Every `render_row(Locale::En, &r.msg)` in the tests becomes `render_row(Locale::En, &r)`; every `describe_event(..).msg` becomes `describe_event(..)`. Let the compiler enumerate them.

- [ ] **Step 5: Update `event_log.rs`**

```rust
use dioxus::prelude::*;

use crate::i18n::use_locale;
use crate::net::view_model::{render_row, RowMsg};

#[component]
pub fn EventLog(rows: Signal<Vec<RowMsg>>) -> Element {
    // A tracked read of the locale signal — this is what re-renders every already-received row
    // when the picker changes language, without the rows themselves being rebuilt.
    let locale = use_locale();
    rsx! {
        div { class: "log",
            for r in rows.read().iter() {
                div { class: "row {r.class()}", "{render_row(locale, r)}" }
            }
        }
    }
}
```

- [ ] **Step 6: Update `app.rs`**

At `:16-19`, drop `LogRow` from the import and add `RowMsg`:

```rust
use crate::net::view_model::{
    can_demote, can_promote, client_error_row, describe_event, error_row, ClientText, ConnState,
    RowMsg,
};
```

At `:51`: `let mut rows = use_signal(Vec::<RowMsg>::new);`

No other `app.rs` change — the seven push sites already push whatever the constructors return.

- [ ] **Step 7: Run the tests**

Run: `cd ui-dioxus && cargo test --features desktop`
Expected: PASS — 179 tests (178 from Task 1 + `row_classes_are_pinned_per_variant`).

Run: `cd ui-dioxus && cargo clippy --features desktop --all-targets`
Expected: no new warnings.

Run: `cd ui-dioxus && cargo build --target wasm32-unknown-unknown --features web`
Expected: PASS.

- [ ] **Step 8: Format and commit**

```bash
cd ui-dioxus && cargo fmt --all && cd ..
git add ui-dioxus/src/net/view_model.rs ui-dioxus/src/components/event_log.rs ui-dioxus/src/app.rs
git commit -m "ui-dioxus: derive the event-log row class and delete LogRow"
```

---

### Task 3: localize the sidecar-spawn failure's framing

**Files:**
- Modify: `ui-dioxus/src/i18n/catalog.rs` (one new key; extend `protocol_identifiers_survive_translation`)
- Modify: `ui-dioxus/src/net/view_model.rs` (`ClientText::Authored` shape + constructors + `render_row`)
- Modify: `ui-dioxus/src/desktop_boot.rs:35-45` (`BootOutcome`), `:144-154` (`boot()`'s spawn-failure arm)
- Modify: `ui-dioxus/src/app.rs:94` and `:454-457`
- Test: `ui-dioxus/src/net/view_model.rs`, `ui-dioxus/src/i18n/catalog.rs`

**Interfaces:**
- Consumes: `ClientText::Passthrough(SeamError)` (Task 1); `client_error_row(..) -> RowMsg` (Task 2).
- Produces: `Msg::SidecarSpawnFailed`; `ClientText::Authored { msg: Msg, args: Vec<(String, String)> }` with `ClientText::authored(Msg)` and `ClientText::authored_with(Msg, Vec<(String, String)>)`; `BootOutcome::SpawnFailed { bin: String, detail: String }`.

- [ ] **Step 1: Write the failing tests**

Add to `net/view_model.rs`'s `mod tests`:

```rust
    #[test]
    fn sidecar_spawn_failure_localizes_its_framing() {
        // i18n spec §2 as amended by the 2026-08-01 type-design spec §3: the sidecar-spawn failure
        // is interface copy (it tells the user auto-connect did not happen and to use the manual
        // form), so the SENTENCE localizes — but `{bin}` and `{detail}` are a filesystem path and
        // an OS error, so they pass through byte-identically in every locale.
        let row = client_error_row(ClientText::authored_with(
            Msg::SidecarSpawnFailed,
            vec![
                ("bin".to_string(), "/usr/bin/otto-sidecar".to_string()),
                ("detail".to_string(), "No such file or directory".to_string()),
            ],
        ));
        let en = render_row(Locale::En, &row);
        let de = render_row(Locale::De, &row);
        assert_ne!(en, de, "the framing sentence must differ per locale");
        for rendered in [&en, &de] {
            assert!(rendered.contains("/usr/bin/otto-sidecar"), "{rendered}");
            assert!(rendered.contains("No such file or directory"), "{rendered}");
        }
    }

    #[test]
    fn authored_client_text_with_no_args_still_retranslates() {
        // The no-parameter case keeps working through the same single `Authored` variant — an
        // empty `args` is the normal shape, not an error state.
        let row = client_error_row(ClientText::authored(Msg::UrlAndTokenRequired));
        assert_ne!(render_row(Locale::En, &row), render_row(Locale::De, &row));
    }
```

Extend `catalog.rs`'s `protocol_identifiers_survive_translation` with:

```rust
            // `serve` is a CLI sub-command — shared vocabulary with the engine and the docs, and
            // the token a user would grep for. It survives translation like the event-kind names.
            assert!(
                t(loc, Msg::SidecarSpawnFailed).contains("serve"),
                "SidecarSpawnFailed lost the `serve` sub-command in {loc:?}"
            );
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cd ui-dioxus && cargo test --features desktop`
Expected: FAIL to compile — `no variant named \`SidecarSpawnFailed\``, `no function or associated item named \`authored_with\``.

- [ ] **Step 3: Add the catalog key**

In `ui-dioxus/src/i18n/catalog.rs`, in the `// ---- Desktop shell ----` block, directly under `ChooseWorkspaceFolder`:

```rust
    // Localized framing, verbatim payload — the shape `RowServerError` establishes. `{bin}` is a
    // filesystem path and `{detail}` an OS error; both pass through byte-identically. The
    // backtick-quoted `serve` is a CLI sub-command and stays untranslated in every locale (see
    // `protocol_identifiers_survive_translation`). Decision recorded in
    // `docs/superpowers/specs/2026-08-01-ui-dioxus-i18n-type-design.md` §3.
    SidecarSpawnFailed { en: "failed to launch `{bin} serve` sidecar: {detail}", de: "Sidecar `{bin} serve` konnte nicht gestartet werden: {detail}", es: "no se pudo iniciar el sidecar `{bin} serve`: {detail}", hi: "साइडकार `{bin} serve` लॉन्च नहीं हो सका: {detail}", zh: "无法启动 `{bin} serve` 边车进程：{detail}" }
```

- [ ] **Step 4: Reshape `ClientText`**

In `net/view_model.rs`:

```rust
/// A client-side row's payload: authored copy (retranslates on a locale switch) or a passthrough
/// diagnostic (rendered verbatim in every locale — i18n spec §2's boundary rule).
#[derive(Clone, PartialEq, Debug)]
pub enum ClientText {
    /// Authored copy, retranslated on every locale switch. `args` fill the template's `{name}`
    /// placeholders and are rendered VERBATIM — they carry filesystem paths and OS errors, not
    /// copy. One variant rather than a separate parameterized sibling: two variants differing only
    /// in whether a `Vec` is empty is the same representable-illegal-state shape `RowMsg::class()`
    /// exists to avoid.
    Authored { msg: Msg, args: Vec<(String, String)> },
    /// A diagnostic the transport seam produced. Verbatim in every locale, and — since
    /// `SeamError`'s constructor is private to `transport/` — unfabricatable anywhere else.
    Passthrough(SeamError),
}

impl ClientText {
    /// Authored copy with no placeholders — the common case.
    pub fn authored(msg: Msg) -> Self {
        Self::Authored {
            msg,
            args: Vec::new(),
        }
    }

    /// Authored copy framing a payload that cannot be translated (a path, an OS error).
    pub fn authored_with(msg: Msg, args: Vec<(String, String)>) -> Self {
        Self::Authored { msg, args }
    }
}
```

And `render_row`'s `ClientError` arm:

```rust
        RowMsg::ClientError(text) => {
            let message = match text {
                ClientText::Authored { msg, args } => {
                    let pairs: Vec<(&str, &str)> =
                        args.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
                    tf(locale, *msg, &pairs)
                }
                ClientText::Passthrough(e) => e.as_str().to_string(),
            };
            tf(locale, Msg::RowClientError, &[("message", &message)])
        }
```

Update the existing test at `:576` from `ClientText::Authored(Msg::UrlAndTokenRequired)` to `ClientText::authored(Msg::UrlAndTokenRequired)`, and the same call in Task 2's `row_classes_are_pinned_per_variant`.

- [ ] **Step 5: Split `BootOutcome::SpawnFailed`**

In `ui-dioxus/src/desktop_boot.rs`, replace the `SpawnFailed(String)` variant:

```rust
    /// `otto serve` failed to spawn (e.g. `otto` not on `PATH` and `OTTO_BIN` unset/wrong).
    ///
    /// Carries the resolved binary and the OS error SEPARATELY rather than a pre-formatted
    /// English sentence: the caller frames them with `Msg::SidecarSpawnFailed` so the sentence
    /// localizes while both payloads pass through verbatim. The old pre-formatted shape was the
    /// one place in the tree that shipped authored prose as a passthrough diagnostic.
    SpawnFailed { bin: String, detail: String },
```

and the spawn-failure arm in `boot()`:

```rust
        Err(e) => {
            // Surface spawn failure both to the terminal/log and (via the returned variant) to the
            // UI — otherwise a misconfigured `OTTO_BIN` / missing `otto` would silently fall
            // through to the manual form with no explanation. The terminal line stays English:
            // stderr is not UI copy (i18n spec, Scope "Out").
            let detail = e.to_string();
            eprintln!("desktop_boot: failed to launch `{bin} serve` sidecar: {detail}");
            return BootOutcome::SpawnFailed { bin, detail };
        }
```

**Do not quote the string `pub async fn boot()` in any comment you add to this file** — `marker_occurs_exactly_once_in_this_file` scans the whole file including prose. Likewise do not introduce `Command::new` or `.arg(` inside `boot()`'s body; `boot_builds_its_sidecar_command_through_serve_command` bans both.

Note: `bin` is currently used by `spawn_guarded`'s error path only after `serve_command(&bin, ...)` borrows it; moving `bin` into the variant is fine because the borrow has ended. If the compiler disagrees, clone it rather than restructuring.

- [ ] **Step 6: Update `app.rs`**

`:94`:

```rust
            rows.write()
                .push(client_error_row(ClientText::authored(Msg::UrlAndTokenRequired)));
```

`:454-457`:

```rust
                // Spawn failure (missing/misconfigured `otto` binary): surface it so the user knows
                // why auto-connect didn't happen, then fall back to the manual form. Localized
                // framing, verbatim payload — the sentence is interface copy, the binary path and
                // OS error are not.
                BootOutcome::SpawnFailed { bin, detail } => {
                    rows.write().push(client_error_row(ClientText::authored_with(
                        Msg::SidecarSpawnFailed,
                        vec![("bin".to_string(), bin), ("detail".to_string(), detail)],
                    )));
                }
```

- [ ] **Step 7: Run the tests**

Run: `cd ui-dioxus && cargo test --features desktop`
Expected: PASS — 181 tests (179 from Task 2 + the 2 new `view_model` tests). The catalog-integrity tests (`no_message_is_empty_in_any_locale`, `placeholder_sets_match_across_locales`, `every_brace_is_a_closed_placeholder`) cover the new key automatically because they iterate `Msg::ALL`.

Run: `cd ui-dioxus && cargo clippy --features desktop --all-targets`
Expected: no new warnings.

Run: `cd ui-dioxus && cargo build --target wasm32-unknown-unknown --features web`
Expected: PASS. (`desktop_boot` is desktop-only, but `ClientText` is shared.)

- [ ] **Step 8: Verify no `Passthrough` carries prose authored outside `transport/`**

Run: `cd ui-dioxus && grep -rn "ClientText::Passthrough" src/`
Expected: exactly two matches in non-test code — the enum definition and `render_row`'s arm — plus the doc-comment mentions. Every `app.rs` construction site now forwards a seam value with no explicit variant name; the one `desktop_boot`-sourced site is gone. Record the output in the task report.

- [ ] **Step 9: Format and commit**

```bash
cd ui-dioxus && cargo fmt --all && cd ..
git add ui-dioxus/src/i18n/catalog.rs ui-dioxus/src/net/view_model.rs ui-dioxus/src/desktop_boot.rs ui-dioxus/src/app.rs
git commit -m "ui-dioxus: localize the sidecar-spawn failure's framing"
```

---

### Task 4: record the boundary decision where the rule is written down

**Files:**
- Modify: `docs/superpowers/specs/2026-07-31-ui-dioxus-i18n-design.md` (§2, after "The boundary rule" block)
- Modify: `CLAUDE.md:55-56`

**Interfaces:**
- Consumes: the shipped behavior from Tasks 1–3.
- Produces: nothing consumed by code.

- [ ] **Step 1: Amend the i18n design spec**

In `docs/superpowers/specs/2026-07-31-ui-dioxus-i18n-design.md`, immediately after the "**Named future upgrade:**" paragraph at the end of §2's "The boundary rule" subsection, insert:

```markdown
> **Amended 2026-08-01 (issue #120 item 3).** Two changes to the above, both shipped:
>
> 1. The "Named future upgrade" landed. The transport seam's `String` error is now
>    `transport::SeamError`, a newtype whose constructor is `pub(in crate::transport)`, and
>    `ClientText` is `{ Authored { msg, args }, Passthrough(SeamError) }`. The boundary is no
>    longer a convention re-decided at each call site: no module outside `transport/` can
>    construct a `Passthrough`.
> 2. **The sidecar-spawn failure is now translated framing over a verbatim payload**, moving it
>    out of the "Crate-authored technical diagnostics" row above. The rule did not cleanly settle
>    it and both readings were defensible; the deciding fact is that it is not on the transport
>    seam at all — it is produced by the desktop shell's boot path before any socket exists, and
>    its job is to tell the user auto-connect did not happen and to use the manual form. Neither
>    of this section's two justifications applies to it: it does not sit in a stream of
>    engine-originated text (it is the reason there is no stream), and the parts worth carrying
>    into a bug report — the binary path and the OS error — pass through untranslated regardless.
>    It renders via `Msg::SidecarSpawnFailed` with `{bin}` and `{detail}` byte-identical in every
>    locale. The other crate-authored diagnostics in that row (`"socket closed"`,
>    `"workspace rpc failed: HTTP {status}"`, `"unexpected response to List/Read"`) are unchanged
>    and stay untranslated — they ARE on the seam.
>
> Full reasoning: `docs/superpowers/specs/2026-08-01-ui-dioxus-i18n-type-design.md`.
```

- [ ] **Step 2: Narrow the `CLAUDE.md` sentence**

`CLAUDE.md:54-56` currently reads:

```
server-originated text (`EventKind::Log`, `VerifyResult.detail`, `ServerMessage::Error`), protocol
identifiers (`Role` names, `FileEdit`/`Verify`/`TurnComplete`), **and the crate's own
transport/boot diagnostics** all pass through untranslated.
```

Replace with:

```
server-originated text (`EventKind::Log`, `VerifyResult.detail`, `ServerMessage::Error`), protocol
identifiers (`Role` names, `FileEdit`/`Verify`/`TurnComplete`), **and the crate's own transport
diagnostics** all pass through untranslated — the last of those enforced by type, not convention:
the seam's error is a `transport::SeamError` whose constructor is private to `transport/`, so
`ClientText::Passthrough` cannot be handed prose authored anywhere else. The desktop **boot**
diagnostic is the one deliberate exception: the sidecar-spawn failure is interface copy and
renders localized framing around a verbatim `{bin}`/`{detail}` payload
(`docs/superpowers/specs/2026-08-01-ui-dioxus-i18n-type-design.md` §3).
```

- [ ] **Step 3: Verify nothing else states the old rule**

Run: `grep -rn "transport/boot diagnostics" CLAUDE.md README.md docs/`
Expected: no matches.

Run: `grep -rn "LogRow" --include=*.rs --include=*.md . | grep -v '^./docs/superpowers/plans/2026-07-3'`
Expected: matches only in this plan and the two specs' historical prose — no live code reference. Historical plan/spec prose describing what #118 built is left alone; it is a record, not a rule.

- [ ] **Step 4: Commit**

```bash
git add docs/superpowers/specs/2026-07-31-ui-dioxus-i18n-design.md CLAUDE.md
git commit -m "docs: record the sidecar-spawn boundary decision where the rule lives"
```

---

## Out-of-band verification (Phase 5)

- **UI bundle** — `ui-dioxus/` changed, so run `cd ui-dioxus && ./scripts/build-web.sh` and confirm its four bundle-trust guards pass (wasm-opt success, no DWARF, under `MAX_WASM_BYTES`). No dependency was added, so the size should be within noise of the 795,188 B baseline.
- **Wasm test harness** — `cd ui-dioxus && CHROMEDRIVER=$(which chromedriver) cargo test --target wasm32-unknown-unknown --features web`. Baseline is 4 tests; expect 5.
- **Workspace** — `cargo test --workspace`, `cargo clippy --workspace --all-targets`, `cargo fmt --all --check` from the repo root. `ui-dioxus/` is workspace-excluded, so this must be **unchanged** from `main`; a difference means something leaked out of the crate.
- **Fly image / distribution / CI / feature-gated crates** — vacuously satisfied: this change touches no `deploy/`, no `.github/`, and no `candle`/`firecracker` code. State that explicitly rather than skipping it.
- **Desktop smoke** — the `SpawnFailed` path is the one user-visible behavior change. Exercise it: `OTTO_BIN=/nonexistent cargo run --features desktop` from `ui-dioxus/`, pick a folder, and confirm the event log shows the framed message with the binary path and OS error intact. If a desktop session is unavailable in the environment, say so in the report rather than claiming it passed.
