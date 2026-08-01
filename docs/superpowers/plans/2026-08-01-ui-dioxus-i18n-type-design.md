# ui-dioxus i18n Type-Design Follow-ups Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Convert two `ui-dioxus` i18n conventions into compiler-enforced invariants — the event log's CSS class becomes a derived function instead of a second field that can contradict the first, and the translate-vs-passthrough boundary becomes a type only `transport/` can mint — and settle the one boundary judgment the i18n spec left open by localizing the sidecar-spawn failure's framing while passing its payload through verbatim.

**Architecture:** `LogRow` is deleted in favour of `RowMsg::class()`. `ClientText::Authored` grows an args vector so a parameterized catalog key (`Msg::SidecarSpawnFailed`) can frame an untranslatable payload — the shape `Msg::RowServerError` already establishes — which removes the last `ClientText::Passthrough` whose `String` did not come from the transport. Only then does a newtype `SeamError` in `transport/mod.rs`, with a `pub(in crate::transport)` constructor, replace `String` as the transport seam's error type, making `ClientText::Passthrough(SeamError)` unfabricatable outside `transport/`.

**Tech Stack:** Rust (edition 2021 in this crate; workspace toolchain pinned 1.85), Dioxus 0.7. No new dependencies — the bundle budget in `ui-dioxus/scripts/build-web.sh` is unaffected.

**Spec:** `docs/superpowers/specs/2026-08-01-ui-dioxus-i18n-type-design.md` — read it first. This plan implements it exactly.

## Global Constraints

- **`ui-dioxus/` stays workspace-excluded.** `cargo build --workspace` / `cargo test --workspace` must be byte-for-byte unaffected. **No file under `crates/` is touched.** The only files outside `ui-dioxus/` that change are `docs/superpowers/specs/2026-07-31-ui-dioxus-i18n-design.md` and the repo-root `CLAUDE.md`, both in Task 4.
- **No new dependencies.** In particular no `trybuild` (spec Assumption 6). `scripts/build-web.sh`'s `MAX_WASM_BYTES` guard must stay satisfied; every new test is `#[cfg(test)]`, so nothing enters the release wasm.
- **Three build configurations must keep compiling:** `--features desktop`, `--target wasm32-unknown-unknown --features web`, and `--no-default-features` (the pure-seam check — `transport/mod.rs`'s three `#[cfg(not(any(feature = "web", feature = "desktop")))]` fallback arms exist for it).
- **The two redaction sites are NOT a mechanical rewrite.** `web.rs:27` and `web.rs:50` are the only two of the twelve `map_err` sites that are not a plain `e.to_string()`; both are `.map_err(|e| redact_token(&format!("{e:?}")))` and both **must keep `redact_token`**. Dropping it ships the bearer token into the visible event log. See Task 3.
- **`SeamError` gets no `From<String>` / `From<&str>` impl and no `std::error::Error` impl.** A blanket conversion is a public constructor by another name (spec Assumption 4 + §1).
- **The exhaustive matches never gain a wildcard arm.** `t`'s `(Locale, Msg)` match and the new `RowMsg::class()` match are both exhaustive-with-no-`_` on purpose: that is what makes a new variant a compile error rather than a silent gap.
- **`style.css` is not touched.** `row-approval` and `row-meter` have no stylesheet rule today (`style.css:41-46` covers the other six); that is pre-existing, cosmetic, and out of scope.
- **No Claude/AI self-attribution** in any commit message, comment, doc, or PR body.
- Run `cargo fmt --all` before every Rust commit (rustfmt is pinned in `rust-toolchain.toml`). Run it **from inside `ui-dioxus/`** — that crate is its own workspace, so a repo-root `cargo fmt --all` does not reach it.
- Default test command for every task: `cd ui-dioxus && cargo test --features desktop`. **Baseline on `main`: 176 passed, 2 ignored.**
- Wasm compile check for every task: `cd ui-dioxus && cargo build --target wasm32-unknown-unknown --features web`.
- **Every line number in this plan is as of `origin/main`.** Tasks 1 and 2 shift `net/view_model.rs` and `app.rs` by roughly +13 and −11 lines respectively, and `cargo fmt` shifts them again. Each citation is paired with the exact source text it refers to — **anchor on the quoted text, not the number**, and never gate a verification step on a line number (see Task 2 Step 8, which is written as a property check for exactly this reason).

## File Structure

| File | Responsibility |
|---|---|
| `ui-dioxus/src/net/view_model.rs` | **Modify.** Task 1: `RowMsg::class()`, delete `LogRow` + `row()`, three constructors return `RowMsg`. Task 2: `ClientText::Authored { msg, args }` + constructors + `render_row`'s arm. Task 3: `Passthrough(SeamError)`. |
| `ui-dioxus/src/components/event_log.rs` | **Modify.** Task 1: `Signal<Vec<RowMsg>>`; `class: "row {r.class()}"`; `render_row(locale, r)`. |
| `ui-dioxus/src/app.rs` | **Modify.** Task 1: `rows` signal type + imports. Task 2: `:94` and the `SpawnFailed` arm at `:454`. Task 3: nothing. |
| `ui-dioxus/src/i18n/catalog.rs` | **Modify.** Task 2: add `SidecarSpawnFailed` (5 locales); extend `protocol_identifiers_survive_translation`. |
| `ui-dioxus/src/desktop_boot.rs` | **Modify.** Task 2: `BootOutcome::SpawnFailed { bin, detail }`; `boot()` stops pre-formatting the UI sentence. |
| `ui-dioxus/src/transport/mod.rs` | **Modify.** Task 3: add `SeamError`; change `SocketEvent`, `Sink::send`, `connect`, `list_files`, `read_file`; three fallback arms; new `#[cfg(test)] mod tests` with two source-scan guards. |
| `ui-dioxus/src/transport/web.rs` | **Modify.** Task 3: eleven error-construction sites; the two `redact_token` sites keep it. |
| `ui-dioxus/src/transport/desktop.rs` | **Modify.** Task 3: ten error-construction sites. |
| `ui-dioxus/src/web_mount_test.rs` | **Modify.** Task 3: add the wasm behavioral redaction test. |
| `docs/superpowers/specs/2026-07-31-ui-dioxus-i18n-design.md` | **Modify.** Task 4: amend §2 (the rule), §6 (the superseded implementation description), §9 (the prescribed `CLAUDE.md` wording). |
| `CLAUDE.md` | **Modify.** Task 4: narrow the boundary sentence at `:53-58`. |

## Task Order & Rationale

**The ordering is load-bearing, and it is not the order the issue lists the items in.**

`app.rs` has eight `ClientText` construction sites: one `Authored` (`:94`), six `Passthrough` carrying a transport-seam `String` (`:200`, `:222`, `:246`, `:334`, `:354`, `:389`), and a **seventh `Passthrough` at `:454-456`** whose `String` comes from `desktop_boot.rs:150`, not from the seam. If `SeamError` landed first, that seventh site would be a hard type error with no local fix — `SeamError::new` is deliberately unreachable from `app.rs` — and the implementer would be forced to improvise the very change Task 2 owns. So:

- **Task 1 (`LogRow`)** goes first: it is independent of both other items, touches `net/view_model.rs` + `event_log.rs` + `app.rs`'s signal type, and gets those files into their final shape before the `ClientText` work starts.
- **Task 2 (`SidecarSpawnFailed`)** converts `app.rs:454` from `Passthrough` to `Authored`. After it, **every** surviving `Passthrough` carries a seam value — which is exactly the precondition Task 3 needs.
- **Task 3 (`SeamError`)** can then change the seam's error type with `app.rs` needing no edit at all. That is the plan's own check on the ordering: if `app.rs` needs a change in Task 3, something upstream was missed.
- **Task 4 (docs)** runs last so it records what actually shipped.

---

### Task 1: delete `LogRow`; derive the row class from `RowMsg`

**Files:**
- Modify: `ui-dioxus/src/net/view_model.rs:57-67` (delete `LogRow` + `row()`), `:329-399` (`describe_event`/`error_row`/`client_error_row`), and the tests at `:445`, `:736`, `:762`
- Modify: `ui-dioxus/src/components/event_log.rs`
- Modify: `ui-dioxus/src/app.rs:16-19` (imports), `:51` (the signal)
- Test: `ui-dioxus/src/net/view_model.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: nothing from earlier tasks.
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
            (
                RowMsg::AgentStarted {
                    role: "Planner".into(),
                },
                "row-agent",
            ),
            (
                RowMsg::AgentFinished {
                    role: "Planner".into(),
                },
                "row-agent",
            ),
            (
                RowMsg::FileEdit {
                    path: "a.rs".into(),
                    bytes: 1,
                },
                "row-edit",
            ),
            (
                RowMsg::Verify {
                    ok: true,
                    detail: String::new(),
                },
                "row-verify",
            ),
            (RowMsg::Log { message: "hi".into() }, "row-log"),
            (RowMsg::TurnComplete { ok: true }, "row-turn"),
            (
                RowMsg::ApprovalRequest {
                    path: "a.rs".into(),
                },
                "row-approval",
            ),
            (RowMsg::Meter { input: 1, output: 2 }, "row-meter"),
            (
                RowMsg::ServerError {
                    message: "boom".into(),
                },
                "row-error",
            ),
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
Expected: **compile error** — ``no method named `class` found for reference `&RowMsg` ``.

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

In `RowMsg`'s doc comment (`:24-42`), add one sentence after the first paragraph: *"The row's CSS class is not stored — `RowMsg::class()` derives it, so a class cannot disagree with its content."*

- [ ] **Step 4: Update the existing tests that read `.class` / `.msg`**

`:445` → `assert_eq!(r.class(), "row-edit");`
`:736` → `assert_eq!(r.class(), "row-approval");`
`:762` → `assert_eq!(r.class(), "row-meter");`

Every `render_row(Locale::En, &r.msg)` becomes `render_row(Locale::En, &r)`; every `describe_event(..).msg` becomes `describe_event(..)`. Let the compiler enumerate them — do not hunt by hand.

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

No other `app.rs` change — all ten `rows.write().push(..)` sites (`:94`, `:174`, `:178`, `:200`, `:222`, `:246`, `:334`, `:354`, `:389`, `:456`) already push whatever the constructors return.

- [ ] **Step 7: Run the tests**

Run: `cd ui-dioxus && cargo test --features desktop`
Expected: PASS — **177 passed, 2 ignored** (176 baseline + `row_classes_are_pinned_per_variant`).

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

### Task 2: localize the sidecar-spawn failure's framing

**Files:**
- Modify: `ui-dioxus/src/i18n/catalog.rs` (one new key; extend `protocol_identifiers_survive_translation`)
- Modify: `ui-dioxus/src/net/view_model.rs:16-22` (`ClientText`), `:114-120` (`render_row`'s `ClientError` arm), `:576` (existing test), and Task 1's `row_classes_are_pinned_per_variant`
- Modify: `ui-dioxus/src/desktop_boot.rs:35-45` (`BootOutcome`), `:144-154` (`boot()`'s spawn-failure arm)
- Modify: `ui-dioxus/src/app.rs:94` and `:452-457`
- Test: `ui-dioxus/src/net/view_model.rs`, `ui-dioxus/src/i18n/catalog.rs`

**Interfaces:**
- Consumes: `client_error_row(..) -> RowMsg` and `render_row(locale, &RowMsg)` from Task 1.
- Produces: `Msg::SidecarSpawnFailed`; `ClientText::Authored { msg: Msg, args: Vec<(String, String)> }` with `ClientText::authored(Msg)` and `ClientText::authored_with(Msg, Vec<(String, String)>)`; `BootOutcome::SpawnFailed { bin: String, detail: String }`.
- **Postcondition Task 3 depends on:** after this task, every `ClientText::Passthrough` in `app.rs` carries a value that came out of `crate::transport`.

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
                (
                    "detail".to_string(),
                    "No such file or directory".to_string(),
                ),
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

Extend `catalog.rs`'s `protocol_identifiers_survive_translation`, inside its existing `for loc in Locale::ALL` loop:

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
Expected: **compile errors** — ``no variant or associated item named `SidecarSpawnFailed` found for enum `Msg` `` and ``no function or associated item named `authored_with` found for enum `ClientText` ``.

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

All five carry `{bin}` then `{detail}` in that order and all five contain `serve`, which is what the existing `placeholder_sets_match_across_locales` and the new `protocol_identifiers_survive_translation` assertion require.

- [ ] **Step 4: Reshape `ClientText`**

In `net/view_model.rs`, replace the `ClientText` enum and its doc comment:

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
    Authored {
        msg: Msg,
        args: Vec<(String, String)>,
    },
    /// A diagnostic produced on the transport seam. Verbatim in every locale.
    Passthrough(String),
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

(`Passthrough` stays `String` in this task; Task 3 changes it to `SeamError`.)

Update `render_row`'s `ClientError` arm (`:114-120`):

```rust
        RowMsg::ClientError(text) => {
            let message = match text {
                ClientText::Authored { msg, args } => {
                    let pairs: Vec<(&str, &str)> =
                        args.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
                    tf(locale, *msg, &pairs)
                }
                ClientText::Passthrough(s) => s.clone(),
            };
            tf(locale, Msg::RowClientError, &[("message", &message)])
        }
```

Nesting `tf` inside `tf` is safe: `tf` never rescans a substituted value (`tf_never_rescans_substituted_values`).

Update the two existing `ClientText::Authored(...)` tuple-constructions to the new constructor:
- `:576` → `client_error_row(ClientText::authored(Msg::UrlAndTokenRequired))`
- Task 1's `row_classes_are_pinned_per_variant` → `RowMsg::ClientError(ClientText::authored(Msg::UrlAndTokenRequired))`

Also update the `RowMsg` doc comment's third bullet (`:38-40`), which names the old tuple shape:

```rust
/// - `ClientError(ClientText)` carries both kinds: the `Authored { msg, args }` arm IS translated
///   (it is this crate's own copy, e.g. `Msg::UrlAndTokenRequired`, or a localized frame around an
///   untranslatable payload, e.g. `Msg::SidecarSpawnFailed`); only `Passthrough` is verbatim.
```

- [ ] **Step 5: Split `BootOutcome::SpawnFailed`**

In `ui-dioxus/src/desktop_boot.rs`, replace the `SpawnFailed(String)` variant (`:38-40`):

```rust
    /// `otto serve` failed to spawn (e.g. `otto` not on `PATH` and `OTTO_BIN` unset/wrong).
    ///
    /// Carries the resolved binary and the OS error SEPARATELY rather than a pre-formatted
    /// English sentence: the caller frames them with `Msg::SidecarSpawnFailed` so the sentence
    /// localizes while both payloads pass through verbatim. The old pre-formatted shape was the
    /// one place in the tree that shipped authored prose as a passthrough diagnostic.
    SpawnFailed { bin: String, detail: String },
```

and the spawn-failure arm in `boot()` (`:146-153`):

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

Moving `bin` into the variant is fine: `serve_command(&bin, &root, &token)` at `:143` only borrows it, and that borrow has ended by this point. No clone is needed.

**Two source-guard tests in this file constrain what you may write:**
- `marker_occurs_exactly_once_in_this_file` scans the whole file — **prose included** — for the string `pub async fn boot()`. Do not quote that marker in any comment you add.
- `boot_builds_its_sidecar_command_through_serve_command` bans `Command::new` and `.arg(` from `boot()`'s comment-stripped body. Do not introduce either.

- [ ] **Step 6: Update `app.rs`**

`:94-96`:

```rust
            rows.write()
                .push(client_error_row(ClientText::authored(
                    Msg::UrlAndTokenRequired,
                )));
```

`:452-457`:

```rust
                // Spawn failure (missing/misconfigured `otto` binary): surface it so the user knows
                // why auto-connect didn't happen, then fall back to the manual form. Localized
                // framing, verbatim payload — the sentence is interface copy, the binary path and
                // the OS error are not.
                BootOutcome::SpawnFailed { bin, detail } => {
                    rows.write().push(client_error_row(ClientText::authored_with(
                        Msg::SidecarSpawnFailed,
                        vec![("bin".to_string(), bin), ("detail".to_string(), detail)],
                    )));
                }
```

- [ ] **Step 7: Run the tests**

Run: `cd ui-dioxus && cargo test --features desktop`
Expected: PASS — **179 passed, 2 ignored** (177 from Task 1 + the 2 new `view_model` tests). The catalog-integrity tests (`no_message_is_empty_in_any_locale`, `placeholder_sets_match_across_locales`, `every_brace_is_a_closed_placeholder`) pick up the new key automatically because they iterate `Msg::ALL`.

Run: `cd ui-dioxus && cargo clippy --features desktop --all-targets`
Expected: no new warnings.

Run: `cd ui-dioxus && cargo build --target wasm32-unknown-unknown --features web`
Expected: PASS. (`desktop_boot` is desktop-only, but `ClientText` is shared, so this is a real check.)

- [ ] **Step 8: Verify Task 3's precondition**

Run: `cd ui-dioxus && grep -rn "ClientText::Passthrough" src/`

Gate on the **property, not on line numbers** — Steps 4 and 6 have already shifted every line in `view_model.rs` and `app.rs`, and Step 9's `cargo fmt` will shift them again:

- Exactly **six** construction sites, **all in `app.rs`**, each forwarding a binding that came from `crate::transport` (the `connect`/`send`/`list_files`/`read_file` error arms and the `SocketEvent::Message(Err(..))` arm).
- **Zero** construction sites in `desktop_boot.rs`, and **zero** inside the `BootOutcome::SpawnFailed` arm — that site must now read `ClientText::authored_with(Msg::SidecarSpawnFailed, ..)`.
- Plus non-construction mentions, which are expected and fine: `net/view_model.rs`'s `render_row` arm, the passthrough test in the same file, and two doc-comment references (`transport/web.rs`, `net/url.rs`).

Paste the actual output into the task report. If a seventh `app.rs` construction remains, or any construction survives in `desktop_boot.rs`, **stop** — Task 3 will not compile, and `SeamError::new` is deliberately unreachable from those modules, so there is no local fix.

- [ ] **Step 9: Format and commit**

```bash
cd ui-dioxus && cargo fmt --all && cd ..
git add ui-dioxus/src/i18n/catalog.rs ui-dioxus/src/net/view_model.rs ui-dioxus/src/desktop_boot.rs ui-dioxus/src/app.rs
git commit -m "ui-dioxus: localize the sidecar-spawn failure's framing"
```

---

### Task 3: `SeamError` — the transport seam carries a typed diagnostic

**Files:**
- Modify: `ui-dioxus/src/transport/mod.rs`
- Modify: `ui-dioxus/src/transport/web.rs`
- Modify: `ui-dioxus/src/transport/desktop.rs`
- Modify: `ui-dioxus/src/net/view_model.rs` (the `Passthrough` arm + the `:582` test)
- Modify: `ui-dioxus/src/web_mount_test.rs` (append one test)
- Test: same files (`#[cfg(test)] mod tests`, per repo convention — tests live next to code)

**Interfaces:**
- Consumes: Task 2's postcondition — every `ClientText::Passthrough` construction forwards a seam value.
- Produces: `crate::transport::SeamError` with `pub fn as_str(&self) -> &str`, `pub(in crate::transport) fn new(detail: impl Into<String>) -> Self`, `#[cfg(test)] pub fn for_test(detail: impl Into<String>) -> Self`, `impl Display`. Derives `Clone, PartialEq, Eq, Debug`.
- Produces: `SocketEvent::Message(Result<ServerMessage, SeamError>)`; `Sink::send(..) -> Result<(), SeamError>`; `connect(..) -> Result<(Box<dyn Sink>, UnboundedReceiver<SocketEvent>), SeamError>`; `list_files(..) -> Result<Vec<PathBuf>, SeamError>`; `read_file(..) -> Result<Vec<u8>, SeamError>`; `ClientText::Passthrough(SeamError)`.

- [ ] **Step 1: Write the tests**

**Read this before running them.** This task ships three tests with deliberately different before/after behavior — know which is which, or Step 2's signal is meaningless:

| Test | Before this task | After |
|---|---|---|
| `seam_error_has_no_crate_wide_constructor` | **FAILS** (runtime panic — `SeamError` does not exist yet) | passes |
| `web_socket_error_paths_still_redact_the_bearer_token` | **passes** | must still pass |
| `connect_error_redacts_the_bearer_token` (wasm) | **passes** | must still pass |

The last two are *regression guards*, not red-then-green tests: the redaction they pin already works, and the whole point is that it survives a rewrite that touches those exact lines. Green-before-and-after is the correct outcome. Verified empirically against Chrome during planning: `connect("ws://[::1/ws?token=supersecret")` returns `Err` whose text is ``SyntaxError: Failed to construct 'WebSocket': The URL 'ws://[::1/ws?token=<redacted>' is invalid.``

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
```

Append to `ui-dioxus/src/web_mount_test.rs` (the module is already gated `#[cfg(all(test, feature = "web", target_arch = "wasm32"))]`, so a bare `#[wasm_bindgen_test]` is correct):

```rust
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
```

- [ ] **Step 2: Run the tests to establish the before-state**

Run: `cd ui-dioxus && cargo test --features desktop transport::`
Expected: **compiles**, then `seam_error_has_no_crate_wide_constructor` FAILS at runtime with ``SeamError's inherent impl block`` (the `.expect` on `.nth(1)`), and `web_socket_error_paths_still_redact_the_bearer_token` PASSES. Both outcomes are correct — record them.

Run: `cd ui-dioxus && CHROMEDRIVER=$(which chromedriver) cargo test --target wasm32-unknown-unknown --features web connect_error_redacts`
Expected: **PASSES already** — `err` is a `String` today and `String::as_str` exists, so it compiles, and the redaction is already in place. This is the guard's baseline; it must stay green through Step 8.

- [ ] **Step 3: Add `SeamError` to `transport/mod.rs`**

Insert directly above the `SocketEvent` enum (keep the module doc comment at the top of the file):

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

Then change the seam signatures in the same file:

- `SocketEvent::Message(Result<ServerMessage, String>)` → `Message(Result<ServerMessage, SeamError>)`
- `fn send(&self, cmd: &Command) -> Result<(), String>` → `-> Result<(), SeamError>`
- `connect`'s return type `..., String>` → `..., SeamError>`
- `list_files` → `Result<Vec<PathBuf>, SeamError>`; `read_file` → `Result<Vec<u8>, SeamError>`

And each of the three `#[cfg(not(any(feature = "web", feature = "desktop")))]` fallback arms, currently `Err("no transport feature enabled (build with --features web or --features desktop)".to_string())`:

```rust
        Err(SeamError::new(
            "no transport feature enabled (build with --features web or --features desktop)",
        ))
```

- [ ] **Step 4: Convert `transport/web.rs`'s eleven error sites**

Extend the existing `use super::{Sink, SocketEvent};` at `:15` to `use super::{SeamError, Sink, SocketEvent};`. Then, by current line number:

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

Change `rpc`'s return type to `Result<WorkspaceResponse, SeamError>`, and `connect_impl`/`list_files_impl`/`read_file_impl`'s to match `mod.rs`.

**Lines 27 and 50 keep `redact_token`.** Do not "simplify" them to `e.to_string()`.
**Do not write `{e:?}` in any new comment in `web.rs`** — `web_socket_error_paths_still_redact_the_bearer_token` scans every line containing it and requires `redact_token` on the same line. (Both rewritten lines stay under rustfmt's 100-column `max_width`, so `cargo fmt` will not split them.)

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

In `ui-dioxus/src/net/view_model.rs`, add `use crate::transport::SeamError;` to the imports, then change the variant Task 2 left as `String`:

```rust
    /// A diagnostic the transport seam produced. Verbatim in every locale, and — since
    /// `SeamError`'s constructor is private to `transport/` — unfabricatable anywhere else.
    Passthrough(SeamError),
```

`render_row`'s `Passthrough` arm becomes:

```rust
                ClientText::Passthrough(e) => e.as_str().to_string(),
```

Update the existing test at `:582`:

```rust
        let passthrough = client_error_row(ClientText::Passthrough(SeamError::for_test(
            "socket closed",
        )));
```

**`app.rs` needs no change in this task.** Its six `Passthrough` sites forward whatever the seam handed them and never name the error type. If the compiler disagrees, Task 2's Step 8 precondition was not met — stop and report rather than reaching for a workaround.

- [ ] **Step 7: Run the host tests**

Run: `cd ui-dioxus && cargo test --features desktop`
Expected: PASS — **181 passed, 2 ignored** (179 from Task 2 + the 2 new `transport::tests`).

Run: `cd ui-dioxus && cargo clippy --features desktop --all-targets`
Expected: no new warnings.

- [ ] **Step 8: Run the wasm build and the wasm tests**

Run: `cd ui-dioxus && cargo build --target wasm32-unknown-unknown --features web`
Expected: PASS.

Run: `cd ui-dioxus && CHROMEDRIVER=$(which chromedriver) cargo test --target wasm32-unknown-unknown --features web`
Expected: PASS — **5 tests** (4 baseline + `connect_error_redacts_the_bearer_token`).

If `chromedriver` or a version-matched `wasm-bindgen-test-runner` is unavailable, report that in the task report rather than deleting the test — the host source-scan still covers the regression, and the wasm test is expected to be harness-only.

- [ ] **Step 9: Prove the boundary is the compiler's, not the test's (manual; do not commit)**

Temporarily add `let _ = crate::transport::SeamError::new("prose");` at the top of `describe_event` in `net/view_model.rs`, then run `cd ui-dioxus && cargo build --features desktop`.
Expected: ``error[E0624]: associated function `new` is private``. Then **revert that line** and confirm `git status --porcelain` shows no stray change. Record the observed error code in the task report.

- [ ] **Step 10: Format and commit**

```bash
cd ui-dioxus && cargo fmt --all && cd ..
git add ui-dioxus/src/transport/ ui-dioxus/src/net/view_model.rs ui-dioxus/src/web_mount_test.rs
git commit -m "ui-dioxus: make the transport seam carry a typed SeamError"
```

---

### Task 4: record the boundary decision where the rule is written down

The i18n design spec states the boundary rule in three places and `CLAUDE.md` in one. Task 2 changed the rule, so all four need amending — §9 in particular *prescribes* the `CLAUDE.md` wording, so leaving it would make the spec instruct a future contributor to restore the superseded sentence.

**Files:**
- Modify: `docs/superpowers/specs/2026-07-31-ui-dioxus-i18n-design.md` — §2 (the "Named future upgrade" paragraph at `:224-227`), §6 (`:511-519`), §9 (`:630-637`)
- Modify: `CLAUDE.md:53-58`

**Interfaces:**
- Consumes: the shipped behavior from Tasks 1–3.
- Produces: nothing consumed by code.

- [ ] **Step 1: Amend §2 — the rule itself**

In `docs/superpowers/specs/2026-07-31-ui-dioxus-i18n-design.md`, immediately after the "**Named future upgrade:**" paragraph at the end of §2's "The boundary rule" subsection, insert:

```markdown
> **Amended 2026-08-01 (issue #120).** Two changes to the above, both shipped:
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
>    of this section's two justifications applies: it does not sit in a stream of
>    engine-originated text (it is the reason there is no stream), and the parts worth carrying
>    into a bug report — the binary path and the OS error — pass through untranslated regardless.
>    It renders via `Msg::SidecarSpawnFailed` with `{bin}` and `{detail}` byte-identical in every
>    locale. The other crate-authored diagnostics in that row (`"socket closed"`,
>    `"workspace rpc failed: HTTP {status}"`, `"unexpected response to List/Read"`) are unchanged
>    and stay untranslated — they ARE on the seam.
>
> Full reasoning: `docs/superpowers/specs/2026-08-01-ui-dioxus-i18n-type-design.md`.
```

- [ ] **Step 2: Amend §6 — the superseded implementation description**

`:511-519` describes `client_error_row`'s call sites (`:511-516`) and `LogRow`'s derives (`:518-519`) as of #118. It is a historical record, so do not rewrite it — append one blockquote directly beneath `:519`:

```markdown
> **Superseded 2026-08-01 (issue #120).** `LogRow` no longer exists — `RowMsg::class()` derives the
> CSS class and the signal is `Signal<Vec<RowMsg>>`. The desktop `SpawnFailed` arm is no longer a
> `Passthrough`: it is `Authored { msg: Msg::SidecarSpawnFailed, args }`, leaving six `Passthrough`
> call sites, all carrying a `transport::SeamError`. See
> `docs/superpowers/specs/2026-08-01-ui-dioxus-i18n-type-design.md` §§1-3.
```

- [ ] **Step 3: Amend §9 — the prescribed `CLAUDE.md` wording**

§9's first bullet (`:630-637`) tells a future contributor what `CLAUDE.md` must say, including "***and the crate's own transport/boot diagnostics*** pass through untranslated", and warns that "a doc line that restates the wrong rule is worse than none". Replace `transport/boot diagnostics` with `transport diagnostics` on `:633` and append one sentence after `:637`:

```markdown
  As amended 2026-08-01, the carve-out is **transport** diagnostics only — the desktop boot
  diagnostic (the sidecar-spawn failure) is interface copy and IS translated, with its `{bin}` and
  `{detail}` payloads passing through verbatim.
```

- [ ] **Step 4: Narrow the `CLAUDE.md` sentence**

`CLAUDE.md:53-58` currently reads (note that line 56 continues past the bolded phrase — do not drop its tail):

```
translated; failure diagnostics carried on the transport's `Result<_, String>` seam are not*. So
server-originated text (`EventKind::Log`, `VerifyResult.detail`, `ServerMessage::Error`), protocol
identifiers (`Role` names, `FileEdit`/`Verify`/`TurnComplete`), **and the crate's own
transport/boot diagnostics** all pass through untranslated. Locale follows the browser/OS by
default; a picker in the status strip overrides it and persists the choice (`localStorage` on web,
the OS config dir on desktop). See `docs/superpowers/specs/2026-07-31-ui-dioxus-i18n-design.md`.
```

Replace with:

```
translated; failure diagnostics carried on the transport's `Result<_, SeamError>` seam are not*. So
server-originated text (`EventKind::Log`, `VerifyResult.detail`, `ServerMessage::Error`), protocol
identifiers (`Role` names, `FileEdit`/`Verify`/`TurnComplete`), **and the crate's own transport
diagnostics** all pass through untranslated — the last enforced by type rather than convention:
`transport::SeamError`'s constructor is private to `transport/`, so `ClientText::Passthrough`
cannot be handed prose authored anywhere else. The desktop **boot** diagnostic is the one
deliberate exception: the sidecar-spawn failure is interface copy and renders localized framing
around a verbatim `{bin}`/`{detail}` payload. Locale follows the browser/OS by
default; a picker in the status strip overrides it and persists the choice (`localStorage` on web,
the OS config dir on desktop). See `docs/superpowers/specs/2026-07-31-ui-dioxus-i18n-design.md`
and `docs/superpowers/specs/2026-08-01-ui-dioxus-i18n-type-design.md`.
```

- [ ] **Step 5: Verify no live rule still states the superseded wording**

Run: `grep -rn "transport/boot diagnostics" CLAUDE.md README.md docs/`

Before Task 4 this returns **9 matches in 5 files**. After Steps 1–4 it must return **7 matches in 4 files**, all of them legitimate:

| File | Matches | Why it stays |
|---|---|---|
| `CLAUDE.md` | **0** | Step 4 narrowed it. This is the assertion that matters. |
| `docs/superpowers/specs/2026-07-31-ui-dioxus-i18n-design.md` | **1** (`:515`, in §6) | Step 2 deliberately does not rewrite §6 — it appends a "Superseded" blockquote beside it. The historical sentence stays. Step 3 removes the §9 occurrence, so `:633` must be gone. |
| `docs/superpowers/specs/2026-08-01-ui-dioxus-i18n-type-design.md` | **1** (`:67`) | The type-design spec *quotes* the old wording in its Scope section to say what it is changing. |
| `docs/superpowers/plans/2026-07-31-ui-dioxus-i18n.md` | **2** | The #118 plan — a historical record, correctly left alone. |
| `docs/superpowers/plans/2026-08-01-ui-dioxus-i18n-type-design.md` | **3** | This plan's own prose. |

Paste the output into the task report. The only failure conditions are a surviving match in `CLAUDE.md` or a second match in `2026-07-31-ui-dioxus-i18n-design.md`.

Run: `grep -rn "LogRow" --include='*.rs' ui-dioxus/`
Note the quoting — zsh expands a bare `--include=*.rs` and aborts with `no matches found`.

Expected: **exactly two matches, both comments, both in `ui-dioxus/src/net/view_model.rs`** — the `class()` doc block and the `row_classes_are_pinned_per_variant` comment. Task 1 *prescribes* both: each explains what `LogRow` was and why deriving the class replaced it, which is the rationale a future reader needs. Zero matches would mean someone stripped that rationale.

**No `LogRow` may remain as a type reference** — no `struct LogRow`, no `Vec<LogRow>`, no `-> LogRow`. Confirm with:

```bash
grep -rn "LogRow" --include='*.rs' ui-dioxus/ | grep -v '^\s*[^:]*:[0-9]*:\s*//'
```

Expected: no output. (`.md` files under `docs/` still mention `LogRow` in historical plan and spec prose; that is a record of what #118 built, not a live rule, and is left alone.)

- [ ] **Step 6: Commit**

```bash
git add docs/superpowers/specs/2026-07-31-ui-dioxus-i18n-design.md CLAUDE.md
git commit -m "docs: record the sidecar-spawn boundary decision where the rule lives"
```

---

## Out-of-band verification (Phase 5)

- **UI bundle** — `ui-dioxus/` changed, so run `cd ui-dioxus && ./scripts/build-web.sh` and confirm its four bundle-trust guards pass (wasm-opt success, no DWARF, under `MAX_WASM_BYTES` = 1,200,000). No dependency was added, so the size should be within noise of the 795,188 B baseline.
- **Wasm test harness** — `cd ui-dioxus && CHROMEDRIVER=$(which chromedriver) cargo test --target wasm32-unknown-unknown --features web`. Baseline 4 tests; expect 5.
- **Workspace** — `cargo test --workspace`, `cargo clippy --workspace --all-targets`, `cargo fmt --all --check` from the repo root. `ui-dioxus/` is workspace-excluded, so these must be **unchanged** from `main`; a difference means something leaked out of the crate.
- **Fly image / distribution / CI / feature-gated crates** — vacuously satisfied: no `deploy/`, no `.github/`, no `candle`/`firecracker` code is touched. State that explicitly rather than skipping it.
- **Desktop smoke** — the `SpawnFailed` path is the one user-visible behavior change. Exercise it: from `ui-dioxus/`, `OTTO_BIN=/nonexistent cargo run --features desktop`, pick a folder, and confirm the event log shows the framed message with the binary path and OS error intact. If no desktop session is available in the environment, say so in the report rather than claiming it passed.
