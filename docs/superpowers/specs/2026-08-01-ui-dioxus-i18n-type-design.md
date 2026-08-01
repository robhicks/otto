# ui-dioxus i18n type-design follow-ups

> **Status:** IMPLEMENTED — shipped in [#121](https://github.com/robhicks/otto/pull/121), together
> with the two review rounds that followed it (redaction moved into `SeamError::new`, the type
> moved to its own module so the constructor is the only door, the arity gap closed, and three
> tests replaced after review showed them vacuous or evadable — see "As shipped" below).
> Three type-design changes deferred from the #118 review: make the
> translate-vs-passthrough boundary a type the compiler enforces, delete a struct whose second
> field is a total function of its first, and settle the one boundary judgment the i18n spec's
> rule did not cleanly decide.
> **Implements:** [#120](https://github.com/robhicks/otto/issues/120).
> **Depends on:** the i18n layer (IMPLEMENTED) — `docs/superpowers/specs/2026-07-31-ui-dioxus-i18n-design.md`.

`ui-dioxus/`'s i18n layer shipped in [#118](https://github.com/robhicks/otto/pull/118) with the
boundary rule from that spec's §2 stated in prose and enforced by convention. Three follow-ups from
the type-design and security reviews were deferred to keep #118 scoped. None is a defect; each
converts a convention into something the compiler checks, or writes down a judgment the rule left
open. This spec picks the exact shapes.

---

## Premise corrections

The issue is accurate about the problems. Two of its *suggested* shapes do not survive contact with
the code and are corrected here.

1. **`pub(crate) fn from_seam` does not enforce anything.** The issue's sketch is

   ```rust
   // in transport/
   pub struct SeamError(String);
   impl SeamError { pub(crate) fn from_seam(s: String) -> Self { Self(s) } }
   ```

   `pub(crate)` is visible to **every module in `ui-dioxus`**, including `net::view_model`,
   `app.rs`, and `components/` — precisely the modules the change exists to lock out. The stated
   goal ("`net::view_model` and the components then cannot fabricate one") is not met by that
   visibility. The enforcing visibility is `pub(in crate::transport)`, and — because Rust's
   visibility qualifiers must name an *ancestor* module — `SeamError` must therefore live in
   `transport/`, not in `net/`.

2. **A private constructor alone is not sufficient; the seam's error type has to change.** With a
   `pub(in crate::transport)` constructor and the seam still returning `Result<_, String>`,
   `app.rs` would hold a `String` and have no way to turn it into a `SeamError` — the genuine
   transport call sites would stop compiling. (The issue says "seven"; the precise count is **six**
   `Passthrough` sites carrying a transport-seam value — `app.rs:200`, `:222`, `:246`, `:334`,
   `:354`, `:389` — plus the seventh at `:456`, which is the sidecar-spawn site §3 converts to
   `Authored`. `app.rs:94` is the one existing `Authored`, for eight `ClientText` constructions
   total.) The change that both enforces the boundary *and* leaves those sites working is to make
   the transport seam itself return `SeamError`:
   `Sink::send`, `connect`, `list_files`, `read_file`, and `SocketEvent::Message`'s `Result` all
   carry `SeamError` instead of `String`. `app.rs` then never constructs one — it only forwards
   what the seam handed it. This is exactly the "Named future upgrade" the i18n spec's §2 already
   records; §1 below adopts it.

3. **`ClientText::Authored(Msg)` cannot express item 3.** The proposed
   `Msg::SidecarSpawnFailed` — `"failed to launch \`{bin} serve\` sidecar: {detail}"` — is a
   *parameterized* authored message, and `Authored(Msg)` carries no args. §3 changes the variant to
   carry them (the shape the i18n spec's §2 named: `Authored(Msg, args)`), rather than adding a
   second, overlapping authored variant.

---

## Scope

**In:** `ui-dioxus/src/transport/{mod,web,desktop}.rs`, `ui-dioxus/src/net/view_model.rs`,
`ui-dioxus/src/components/event_log.rs`, `ui-dioxus/src/app.rs`,
`ui-dioxus/src/desktop_boot.rs`, `ui-dioxus/src/i18n/catalog.rs`; an amendment to the i18n design
spec recording the §3 decision; **and the corresponding sentence in the repo-root `CLAUDE.md`** —
its UI paragraph currently states the boundary as *"server-originated text …, protocol identifiers
…, **and the crate's own transport/boot diagnostics** all pass through untranslated"*
(`CLAUDE.md:56`). §3 makes the boot diagnostic translated, so that sentence becomes wrong on merge
and must be narrowed to the transport diagnostics alone. The rule is written down in two places;
both are in scope.

**Out:** any workspace crate (`ui-dioxus/` is workspace-excluded and depends only on
`otto-protocol`, so `cargo test --workspace` is neither exercised by nor exercises this change);
the wire protocol; new locales; the `{bin}`/`{detail}` payloads themselves (they stay verbatim);
localizing the remaining transport diagnostics (`"socket closed"`,
`"workspace rpc failed: HTTP {status}"`, `"unexpected response to List/Read"`) — the i18n spec's §2
rule keeps those untranslated and this change reinforces that, it does not revisit it.

---

## Goal & success criteria

Convert two conventions into compiler-checked invariants and settle one open judgment, with no
change to what any user sees except the sidecar-spawn failure, which becomes localized framing
around an unchanged payload.

- `ClientText::Passthrough` can be constructed only from a value the `transport` module produced —
  attempting it from `net/`, `app.rs`, `components/`, or `desktop_boot.rs` is a compile error.
- `LogRow` no longer exists; a row's CSS class is derived from its `RowMsg`, so a row with a class
  that contradicts its content is unrepresentable.
- The sidecar-spawn failure renders localized framing in all five locales with `{bin}` and
  `{detail}` byte-identical across them, and `desktop_boot.rs` authors no user-facing prose.
- `cd ui-dioxus && cargo test --features desktop` passes (176 tests today, plus the new ones), and
  `cargo build --target wasm32-unknown-unknown --features web` compiles.
- Every `ClientText::Passthrough` value in the tree is a `SeamError` minted inside `transport/`.
  (Deliberately **not** "no `Passthrough` carries authored prose": several `SeamError` payloads are
  crate-authored English — `"socket closed"`, `"workspace rpc failed: HTTP {status}"`,
  `"unexpected response to List/Read"`, `"no transport feature enabled …"` — and Scope explicitly
  keeps them untranslated. The checkable property is *provenance*, which is exactly what §1's
  private constructor enforces, not authorship.)
- `transport/web.rs`'s bearer-token redaction survives the seam's type change: a `SeamError` from
  `connect_impl` contains `token=<redacted>`, never the token. Guarded by a host source-scan that
  runs in the default `--features desktop` gate **and** by a wasm behavioral test that does not
  (§1 Testing explains why both).

---

## 1. `SeamError` — the passthrough boundary becomes a type

### The problem

`ClientText::Passthrough(String)` documents a policy it cannot enforce. The incentive runs the
wrong way: every transport call site already holds a `String` from a `Result<_, String>`, so
`Passthrough` is the path of least resistance, while `Authored` requires stopping to add a catalog
key. The reviewer's in-tree proof is `desktop_boot.rs:150`, which builds
`format!("failed to launch \`{bin} serve\` sidecar: {e}")` — authored English prose wrapped around
an OS error — and ships it as `Passthrough`. Policy-conformant under the current rule, and a
demonstration that the type cannot tell "text the engine sent me" from "English prose I wrote."

### Decision

Add a newtype in `transport/mod.rs` whose only constructor is private to that module tree, and
change the transport seam to carry it:

```rust
// transport/mod.rs
/// A failure diagnostic produced on the transport seam.
///
/// The i18n boundary (i18n spec §2) says these render verbatim in every locale. That was a
/// convention; this type is the enforcement. The constructor is `pub(in crate::transport)`, so
/// only `transport/` and its target impls can mint one — `net/`, `app.rs`, `components/`, and
/// `desktop_boot.rs` can hold and display a `SeamError` but can never fabricate one out of
/// crate-authored prose.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SeamError(String);

impl SeamError {
    pub(in crate::transport) fn new(detail: impl Into<String>) -> Self {
        Self(detail.into())
    }

    /// The diagnostic text, for rendering. Read-only by construction.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Mint a `SeamError` in a test that is exercising a *consumer* of the seam rather than the
    /// seam itself. `cfg(test)` so no production path can reach it.
    #[cfg(test)]
    pub fn for_test(detail: impl Into<String>) -> Self {
        Self(detail.into())
    }
}

impl std::fmt::Display for SeamError { /* writes self.0 */ }
```

and the seam signatures become:

```rust
pub enum SocketEvent { Message(Result<ServerMessage, SeamError>), Closed, Errored }
pub trait Sink { fn send(&self, cmd: &Command) -> Result<(), SeamError>; fn close(&self); }
pub fn connect(ws_url: &str) -> Result<(Box<dyn Sink>, UnboundedReceiver<SocketEvent>), SeamError>;
pub async fn list_files(http_base: &str, token: &str) -> Result<Vec<PathBuf>, SeamError>;
pub async fn read_file(http_base: &str, token: &str, path: PathBuf) -> Result<Vec<u8>, SeamError>;
```

`net/view_model.rs` then holds `Passthrough(SeamError)`.

### Why this and not the alternatives

- **`pub(crate)` constructor, seam still `String`** — the issue's literal sketch. Rejected: see
  Premise correction 1. It moves the convention into a differently-named function without narrowing
  who may call it.
- **`SeamError` in `net/`** — rejected: `pub(in crate::transport)` cannot be written from a module
  that is not an ancestor of `transport`, so the constructor would have to be `pub(crate)` again.
- **A sealed trait / private marker parameter** — more machinery for the same guarantee. A newtype
  with a module-private constructor is the smallest thing that works.
- **Making `SeamError` non-`Clone`** — considered, since a diagnostic is naturally move-only. Rejected:
  `ClientText`/`RowMsg` derive `Clone` + `PartialEq` for Dioxus signal semantics, and
  `app.rs`'s `SocketEvent::Message(Err(detail))` arm holds the value behind a reference. `Clone` is
  required; there is no invariant a clone could break.

### Security property — the two redacting sites are NOT a mechanical rewrite

`transport/web.rs` redacts the bearer token out of two errors before the `String` leaves the
transport, because `ws_url` carries the token as a query parameter and a rejected URL comes back as
a `SyntaxError` that quotes the URL in full:

- `web.rs:27` — `WebSink::send`: `.map_err(|e| redact_token(&format!("{e:?}")))`
- `web.rs:50` — `connect_impl`: `.map_err(|e| redact_token(&format!("{e:?}")))?`

**These two sites must become `SeamError::new(redact_token(&format!("{e:?}")))`, not
`SeamError::new(e.to_string())`.** They are the only two of the twelve `map_err` sites that are not
a plain `e.to_string()`, and rewriting them uniformly with the other ten would delete the redaction
and ship the bearer token into the event log — the surface most likely to be pasted into a bug
report. Nothing in the suite would catch it today: `redact_token`'s tests (`net/url.rs:168-213`)
exercise the pure function only, never a call site.

Given that, the redaction now happens *inside* the module that owns the constructor, so no other
module can construct a `SeamError` around an unredacted URL either. The change narrows the surface;
it does not add one. `SeamError` carries no `From<String>`/`From<&str>` impl: a blanket conversion
would be a public constructor by another name.

### Testing

- **Two tests, deliberately, because neither alone covers the regression.**
  - `web_socket_error_paths_still_redact_the_bearer_token` — a **host** source-scan in
    `transport/mod.rs` over `include_str!("web.rs")`, asserting every `{e:?}` format in that file
    is on a line that also names `redact_token`, and that there are exactly two such sites. This is
    the one that matters operationally: `transport/mod.rs` compiles under every feature
    combination, so this runs in `cd ui-dioxus && cargo test --features desktop` — the command the
    success criteria name, and the only one a developer runs by default. The regression it guards
    is a *source-level* edit, which is exactly what a source scan can see.
  - `connect_error_redacts_the_bearer_token` — a **wasm** behavioral test in `web_mount_test.rs`
    (`#[cfg(all(test, feature = "web", target_arch = "wasm32"))]`), asserting a `SeamError` from
    `connect_impl` for a scheme-invalid URL carrying `token=<secret>` renders `token=<redacted>`
    and does not contain the secret. This is the real guarantee rather than a proxy for it.

  **Stated plainly: the wasm test is browser-harness-only and is NOT part of the default gate.**
  It needs `wasm-bindgen-test-runner` version-matched to `Cargo.lock`'s `wasm-bindgen` plus a
  webdriver (`.cargo/config.toml`). **Correction, discovered at merge time:** the repo *does* have
  CI as of [#117](https://github.com/robhicks/otto/pull/117) (`.github/workflows/ci.yml`, added to
  `main` after this branch was cut) — but it runs `fmt`/`clippy`/`test` over the **workspace**, and
  `ui-dioxus/` is workspace-excluded, so no `ui-dioxus` test runs in CI at all. The conclusion is
  unchanged and in fact stronger: neither the wasm suite nor the host `--features desktop` suite is
  gated automatically, so a host-runnable guard is what protects the next developer.
- `passthrough_can_only_carry_a_transport_value` — a doc-level compile-fail is not worth a
  `trybuild` dependency for one case; instead the invariant is asserted structurally: `SeamError`'s
  only non-`cfg(test)` constructor is `pub(in crate::transport)`, verified by a source-scan test in
  `transport/mod.rs` (the crate already uses this technique — `desktop_boot.rs`'s
  `boot_code_without_comments` scans its own source).
- The existing `client_error_rows_carry_authored_or_passthrough_text` test keeps its coverage,
  constructing its passthrough via `SeamError::for_test`.

---

## 2. `LogRow` is deleted; the class is derived

### The problem

`LogRow { class: &'static str, msg: RowMsg }` — across all ten `RowMsg` variants, `class` is a
**total, 1:1 function of `msg`**: `AgentStarted`/`AgentFinished` → `row-agent`,
`ServerError`/`ClientError` → `row-error`, and one apiece for the rest. So
`LogRow { class: "row-agent", msg: RowMsg::ServerError { .. } }` compiles — a representable illegal
state. Low stakes (a wrong CSS class), and free to eliminate.

### Decision

```rust
impl RowMsg {
    /// The row's CSS class. A total function of the variant — which is why `LogRow` does not
    /// exist: a struct pairing the two could represent a class that contradicts its content.
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

`LogRow` and the private `row()` helper are deleted. `describe_event`, `error_row`, and
`client_error_row` return `RowMsg`. `app.rs`'s signal becomes `Signal<Vec<RowMsg>>`.
`components/event_log.rs` becomes:

```rust
div { class: "row {r.class()}", "{render_row(locale, r)}" }
```

The match is exhaustive with no wildcard arm, for the same reason the catalog's is: adding a
`RowMsg` variant must be a compile error here, not a row that silently renders unclassed.

### Testing

- `row_classes_are_pinned_per_variant` — one assertion per `RowMsg` variant pinning `class()` to
  its exact expected string, so a typo (`"row-aproval"`) fails rather than silently rendering
  unstyled. Deliberately a **pin against a hardcoded expectation**, not a check against
  `style.css`: `style.css:41-46` defines only `.row-agent`/`.row-edit`/`.row-verify`/`.row-log`/
  `.row-turn`/`.row-error`, so `row-approval` and `row-meter` — both produced by today's
  `describe_event` and preserved unchanged here — have no stylesheet rule and inherit `.row`'s
  colour. That is a **pre-existing** cosmetic gap, out of scope for this change (see Risks); a
  stylesheet-derived test would fail on merge for a reason this PR did not cause.
- The existing `assert_eq!(r.class, "row-edit")`-style assertions become `r.class()`.

---

## 3. The sidecar-spawn failure is localized framing over a verbatim payload

### The judgment, stated

The i18n spec's §2 rule — *interface copy is translated; failure diagnostics carried on the
transport's `Result<_, String>` seam are not* — does not cleanly settle this one, which is why the
issue asks for it to be decided deliberately rather than assumed. Both readings are defensible:

- **Diagnostic.** It reports a failure, and its audience is plausibly a bug report.
- **Interface copy.** It tells the user that auto-connect did not happen and that they should use
  the manual connection form — that is the app describing its own state and directing the user to
  its own controls, which §2's rule explicitly covers ("interface *state*, not only instruction").

### Decision: translate the sentence, pass the payload through

The deciding fact is that **it is not on the transport seam.** §2's rule names one specific
surface: `Sink::send`, `connect`, `list_files`, `read_file` — the `Result<_, String>` values that
share the event log with permanently-English engine output. The sidecar-spawn failure is produced
by the *desktop shell's own boot path* before any socket exists. Its two justifications in §2 do
not apply: it does not sit in a stream of engine-originated text (it is the reason there is no
stream), and the part of it worth carrying into a bug report — the binary path and the OS error —
survives untranslated regardless, because only the framing is localized.

So it takes the shape `Msg::RowServerError` already establishes and §2 blesses: **localized
sentence, verbatim payload.**

```
SidecarSpawnFailed { en: "failed to launch `{bin} serve` sidecar: {detail}", … }
```

`{bin}` (a filesystem path) and `{detail}` (the OS error) are substituted verbatim and are
byte-identical in every locale. The five translations keep the backtick-quoted `` `{bin} serve` ``
literal intact — `serve` is a CLI subcommand, a protocol-identifier-class token under §2, and the
existing `protocol_identifiers_survive_translation` test's rationale applies to it.

This is also what makes §1's boundary airtight rather than merely narrower: after this change,
every remaining `Passthrough` value is literally a `SeamError` the transport produced. Note the
property is **provenance, not authorship** — several surviving `SeamError` payloads are still
crate-authored English (`"socket closed"`, `"workspace rpc failed: HTTP {status}"`,
`"unexpected response to List/Read"`, `"no transport feature enabled …"`), and Scope keeps them
untranslated deliberately. What §3 removes is the only `Passthrough` authored **outside**
`transport/`, which is the case the private constructor could not otherwise reach.

### Shape

`ClientText::Authored` gains args — the shape the i18n spec §2's "Named future upgrade" names:

```rust
#[derive(Clone, PartialEq, Debug)]
pub enum ClientText {
    /// Authored copy. Retranslates on every locale switch. `args` fill the template's
    /// placeholders and are rendered VERBATIM — they carry paths and OS errors, not copy.
    Authored { msg: Msg, args: Vec<(String, String)> },
    /// A diagnostic the transport seam produced. Verbatim in every locale (i18n spec §2), and —
    /// since `SeamError`'s constructor is private to `transport/` — unfabricatable elsewhere.
    Passthrough(SeamError),
}

impl ClientText {
    pub fn authored(msg: Msg) -> Self { Self::Authored { msg, args: Vec::new() } }
    pub fn authored_with(msg: Msg, args: Vec<(String, String)>) -> Self { Self::Authored { msg, args } }
}
```

One authored variant rather than two (`Authored(Msg)` plus a parameterized sibling): two variants
that differ only in whether a `Vec` is empty is the same "representable illegal state" shape §2 of
this spec is deleting. The `authored`/`authored_with` constructors keep the no-arg call site
(`Msg::UrlAndTokenRequired`) as short as it is today.

`render_row`'s `ClientError` arm becomes:

```rust
ClientText::Authored { msg, args } => {
    let pairs: Vec<(&str, &str)> = args.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    tf(locale, *msg, &pairs)
}
ClientText::Passthrough(e) => e.as_str().to_string(),
```

### `desktop_boot` stops authoring prose

```rust
pub enum BootOutcome {
    Cancelled,
    /// `otto serve` failed to spawn. Carries the resolved binary and the OS error SEPARATELY so
    /// the caller can frame them with `Msg::SidecarSpawnFailed` — this variant used to carry a
    /// pre-formatted English sentence, which is exactly the authored-prose-as-`Passthrough` case
    /// issue #120 item 1 was about.
    SpawnFailed { bin: String, detail: String },
    Ready(Child, LaunchParams),
}
```

`boot()`'s existing `eprintln!` stays English and unchanged in substance (terminal/log output is
out of scope per the i18n spec — it is not UI copy); it formats the sentence inline.

`app.rs`:

```rust
BootOutcome::SpawnFailed { bin, detail } => {
    rows.write().push(client_error_row(ClientText::authored_with(
        Msg::SidecarSpawnFailed,
        vec![("bin".into(), bin), ("detail".into(), detail)],
    )));
}
```

### Testing

- `sidecar_spawn_failure_localizes_its_framing` — `render_row` for two locales differs, and both
  contain the `{bin}` and `{detail}` values verbatim.
- The catalog's existing `placeholder_sets_match_across_locales`, `no_message_is_empty_in_any_locale`,
  and `every_brace_is_a_closed_placeholder` cover the new key automatically (they iterate `Msg::ALL`).
- A `serve` sub-command survival assertion added to `protocol_identifiers_survive_translation`.

---

## Assumptions

Every choice made without asking, with its rationale:

1. **The i18n design spec is amended, not superseded — and so is `CLAUDE.md`.** §3's decision is
   recorded as an amendment in `2026-07-31-ui-dioxus-i18n-design.md` §2 (a short "Amended" note
   pointing here), because that file is where a future contributor looks up the boundary rule, and
   the same sentence is narrowed in the repo-root `CLAUDE.md:56`. Rationale: the issue's own closing
   line — "The spec's rule does not cleanly settle it, which is itself a reason to write the answer
   down" — asks for the answer to live where the rule lives, and the rule lives in two places.
2. **Translations are authored, not machine-generated.** The five `SidecarSpawnFailed` strings
   follow the register of the existing catalog entries (lowercase leading word in `en`/`es`,
   sentence-case German noun style, etc.). Rationale: consistency with the 40-odd keys already
   shipped; the catalog is reviewed as a unit.
3. **`SeamError` is `Eq` as well as `PartialEq`.** It wraps a `String`. Rationale: costs nothing,
   and lets a future `HashSet<SeamError>`-style dedup exist without a churn commit.
4. **`Display` but not `std::error::Error`.** Rationale: nothing in the crate uses `?`-conversion
   or `Box<dyn Error>`; adding the trait would invite `From` impls, which are public constructors.
5. **`error_row(&str)` keeps its `&str` parameter.** It carries a `ServerMessage::Error` payload,
   not a transport-seam value — a different provenance that `SeamError` deliberately does not
   claim. Rationale: `SeamError` means "the transport produced this"; widening it to "any untranslated
   text" would restore exactly the ambiguity §1 removes.

   One acknowledged imprecision: the workspace-RPC path (`web.rs:97`, `desktop.rs:108`) returns
   `WorkspaceResponse::Error { message }` — a *server* payload — as a transport `Result` error, so
   it becomes a `Passthrough(SeamError)`. `SeamError` therefore reads as "this value reached the app
   through the transport seam", not "the transport authored it". That is harmless (both provenances
   are untranslated under the i18n spec's §2, and both render verbatim) and is the honest reading of
   the name; it is recorded here so a reviewer does not have to rediscover the tension.
6. **No `trybuild` dependency.** The "cannot fabricate a `Passthrough` elsewhere" invariant is
   verified by a source-scan test rather than a compile-fail fixture. Rationale: `ui-dioxus/`'s
   dependency set is bundle-budget-sensitive (`scripts/build-web.sh` enforces `MAX_WASM_BYTES`), the
   crate has an established source-scan precedent, and a dev-dependency for one assertion is
   disproportionate. Stated as a trade-off: a source scan is weaker than a compile-fail test and can
   be defeated by reformatting; it is chosen knowingly.

---

## Error handling & edge cases

- **`tf` with an unsupplied placeholder** renders `{bin}` / `{detail}` literally. Already the
  designed behavior ("visibly wrong beats silently blank") and already tested; `authored_with`
  supplies both, so this is a guard, not a path.
- **A `bin` path containing braces** (`/opt/{weird}/otto`) is substituted verbatim —
  `tf` never rescans substituted values, covered by `tf_never_rescans_substituted_values`.
- **A `SeamError` containing braces** never reaches `tf` as a template: `Passthrough` renders via
  `as_str()`, and the surrounding `Msg::RowClientError` substitution treats it as a value.
- **`--no-default-features`** (neither `web` nor `desktop`): `transport/mod.rs`'s fallback arms
  construct `SeamError::new(...)` from inside `transport`, so they keep compiling.
- **Empty `args`** on `ClientText::Authored` is the normal no-parameter case, not an error state.

---

## Risks & open questions

- **Churn across the transport impls.** Twelve `map_err` sites (7 in `web.rs`, 5 in `desktop.rs`)
  change. Ten are a plain `map_err(|e| e.to_string())` → `map_err(|e| SeamError::new(e.to_string()))`
  and are genuinely mechanical; **the two redacting sites (`web.rs:27`, `web.rs:50`) are not** — see
  §1's Security property. The compiler finds all twelve, but it cannot tell you that two of them
  must keep `redact_token`, which is why they are enumerated rather than described as a pattern.
  Mitigation: one task, done first, so later tasks build on a compiling seam, with the redaction
  regression test in that same task.
- **`desktop_boot.rs` has source-scanning guard tests that a careless doc comment can break.**
  `boot_builds_its_sidecar_command_through_serve_command` bans `Command::new` and `.arg(` from
  `boot()`'s comment-stripped body, and `marker_occurs_exactly_once_in_this_file` bans a second
  occurrence of the `pub async fn boot()` marker **anywhere in the file, prose included**. The §3
  edit does not break either, but a new doc comment on `SpawnFailed` that quotes the marker would.
- **`row-approval` and `row-meter` have no `style.css` rule** (`style.css:41-46` covers the other
  six). Pre-existing, cosmetic (they inherit `.row`'s colour), and untouched by this change —
  recorded so it is a known gap rather than a surprise, and deliberately not fixed here, since
  neither the issue nor this spec's scope covers the stylesheet.
- **The source-scan test is a weaker guarantee than a compile-fail test** (Assumption 6). If the
  crate ever takes a dev-dependency on `trybuild` for another reason, converting this is a small
  follow-up.
- **§3 is a judgment, not a proof.** A future contributor may disagree that the sidecar failure is
  interface copy. The amendment in the i18n spec records the reasoning so the disagreement is with
  a stated argument rather than with an unexplained precedent.


---

## As shipped

Recorded at merge so the spec matches the code rather than the plan. Four things changed during
implementation and review; each is a correction to this document, not a deviation from it.

1. **Redaction moved into `SeamError::new`.** §1 specified redaction at the two `web.rs` call
   sites, guarded by a source scan. Review showed that scan was evadable (it keyed on the literal
   `{e:?}`, so a differently-named binding slipped past), blind to `desktop.rs`, and brittle enough
   to fail the correct chokepoint refactor. Redaction is now applied by the single constructor, so
   the property is structural. The source scan was deleted rather than patched.

2. **`SeamError` lives in `transport/seam_error.rs`, not `transport/mod.rs`.** §1's Premise
   correction 1 got the visibility argument right for `pub(in crate::transport)` but missed a
   second instance of the same rule: a private *field* is visible to the declaring module **and its
   descendants**, and `transport::{web,desktop}` were descendants — so they could construct
   `SeamError(..)` directly, bypassing the constructor and its redaction. Declared in its own
   module they are siblings, the field is unnameable, and direct construction is `error[E0423]`.
   This is what makes the section's claim literally true.

3. **`ClientText::authored`/`authored_with` gained arity guards.** The spec's §3 shape allowed
   `authored(Msg::SidecarSpawnFailed)`, which renders the literal `{bin}` to a user because `tf`
   emits unsupplied placeholders verbatim. `Msg::has_placeholders()` is derived from the catalog at
   macro-expansion time and both constructors `debug_assert` on it. Keys are `&'static str`. The
   doc comment's original justification — an analogy to `RowMsg::class()` — was unsound and is
   corrected in the code: `class` was derivable from its variant, `args` is not derivable from
   `msg`.

4. **The rule lived in five places, not four.** Scope named the i18n spec's §2/§6/§9 and
   `CLAUDE.md`. The fifth is `ui-dioxus/src/i18n/mod.rs`'s module doc — the one a contributor reads
   while adding a catalog key. The miss was structural: the plan's verification grep was scoped to
   `docs/`, which cannot see the source tree. Both are fixed.

Deferred, with reasoning, to [#122](https://github.com/robhicks/otto/issues/122): a failed
connection currently produces no event-log row at all; a sidecar that dies after spawning is
silent while its stderr diagnosis is read and discarded; plus four smaller hardening items.
