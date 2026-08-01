# Provider base-URL hardening Implementation Plan

> **For agentic workers: REQUIRED SUB-SKILL.** Execute this plan with
> `superpowers:subagent-driven-development` (or `superpowers:executing-plans` when running inline).
> Work task-by-task in order, checking off each `- [ ]` step as it completes. Do not batch tasks.

**Spec:** `docs/superpowers/specs/2026-08-01-provider-base-url-hardening-design.md` — read it
first. This plan implements it exactly.

Closes [#111](https://github.com/robhicks/otto/issues/111) and
[#112](https://github.com/robhicks/otto/issues/112).

## Goal

Never transmit a provider API key in cleartext to a non-loopback host, and never emit a malformed
endpoint, when an operator overrides `OPENAI_BASE_URL` / `DEEPSEEK_BASE_URL`.

## Architecture

One new module, `crates/providers/src/base_url.rs`, holds both halves: `validate_base_url`
(scheme/host policy, `pub`) and `join_url` (slash-safe composition, `pub(crate)`). The shared
`OpenAiCompatibleProvider::complete` uses `join_url`. The engine validates at the trust boundary —
the env read — through a new env-pure `resolve_base_url` seam, and `build_remote` becomes
fallible (`Option`), with both `build_router_with_model` arms degrading to the offline router.

## Tech Stack

Rust (edition 2024, toolchain pinned in `rust-toolchain.toml`). URL parsing via `reqwest::Url` —
a re-export of `url::Url`, available under the crate's existing
`default-features = false, features = ["json", "rustls-tls"]` declaration, **so no new Cargo
dependency is added**. Tests with the in-crate `#[cfg(test)] mod tests` convention; existing HTTP
tests use `wiremock`.

## Global Constraints

- **Dependency flow stays inward.** `base_url.rs` lives in `otto-providers` (an impl crate). No new
  crate edges. `engine-core` is untouched.
- **Determinism is a test invariant.** All env reads stay behind `build_router`/`build_remote`. With
  no env vars set the offline default must be byte-for-byte unchanged.
- **No test may set or remove a process env var.** `crates/engine/src/lib.rs:596-601` carries a
  SAFETY contract forbidding it; the `resolve_base_url` seam exists precisely so it is not needed.
- **Never log or format the API key.** It is not in scope in `base_url.rs`; keep it that way.
- No AI/Claude self-attribution in commits, comments, or docs.
- Run `cargo fmt --all` before every commit. CI runs
  `cargo clippy --workspace --all-targets -- -D warnings`, so warnings are build failures.
- `ui-dioxus/` is workspace-excluded and untouched by this change.

## File Structure

| File | Responsibility |
|---|---|
| `crates/providers/src/base_url.rs` | **Create.** `BaseUrlError`, `validate_base_url`, `join_url`, and their unit tests. |
| `crates/providers/src/lib.rs` | **Modify.** Declare `mod base_url;` and re-export `validate_base_url` + `BaseUrlError`. |
| `crates/providers/src/openai_compatible.rs` | **Modify.** Replace the `format!` endpoint concat with `join_url`; add composition tests. |
| `crates/engine/src/lib.rs` | **Modify.** Add env-pure `resolve_base_url`; `build_remote` returns `Option<Arc<dyn Provider>>`; both router arms handle `None`; unit tests for the seam. |
| `CLAUDE.md` | **Modify.** Document the https-or-loopback constraint on both `*_BASE_URL` vars. |

## Task Order & Rationale

Task 1 creates the leaf module with no callers, so it can be tested in isolation. Task 2 consumes
`join_url` (#112) — independent of the engine. Task 3 consumes `validate_base_url` (#111) and is
last because it changes a function signature that Tasks 1–2 do not touch. Docs land with Task 3,
the task that makes the behavior user-visible.

---

### Task 1: The `base_url` module — validation + slash-safe join

**Files:** `crates/providers/src/base_url.rs` (create), `crates/providers/src/lib.rs` (modify)

**Interfaces:** produces `pub fn validate_base_url(&str) -> Result<(), BaseUrlError>`,
`pub enum BaseUrlError`, and `pub(crate) fn join_url(&str, &str) -> String`. Consumes nothing.

- [ ] Create `crates/providers/src/base_url.rs` with a module doc comment explaining that this is
      the trust boundary for operator-supplied base URLs, and why loopback `http` is permitted.
- [ ] Define `pub enum BaseUrlError { Unparseable(String), UnsupportedScheme { url: String, scheme: String }, InsecureScheme(String), MissingHost(String) }` with `impl Display` and an
      empty `impl std::error::Error`. Never include an API key (none is in scope here).
- [ ] Write the failing tests FIRST in `#[cfg(test)] mod tests`, covering the spec's edge-case
      table exactly: accept `https://api.openai.com`, `https://api.openai.com/v1/`,
      `http://127.0.0.1:8080`, `http://localhost:1234`, `http://[::1]:8080`, `http://2130706433`,
      `http://LOCALHOST`, `https:///foo`; reject `http://api.openai.com`,
      `http://localhost.evil.com`, `http://localhost.`, `http://169.254.169.254`, `ftp://host`,
      `file:///etc/passwd`, `not a url`, `""`, `https://`, `http://`.
      **Do NOT write a test asserting any input yields `MissingHost`** — per the spec it is
      unreachable for http/https and exists only to keep the `host()` match total.
- [ ] Also write failing `join_url` tests: base without slash, with one trailing slash, with many
      (`https://host///`), and with a path prefix (`https://host/v1`), each across both
      `/v1/chat/completions` and `/chat/completions`. Assert the result contains no `//` after the
      scheme's `://`.
- [ ] Run `cargo test -p otto-providers base_url` — expect compile failure / test failure.
- [ ] Implement `validate_base_url` using `reqwest::Url::parse`: map a parse error to
      `Unparseable`; match `url.scheme()` — `"https"` accept, `"http"` accept only when the host is
      loopback, anything else `UnsupportedScheme`. Loopback = `Host::Domain("localhost")` (exact
      equality) | `Host::Ipv4(ip)` where `ip.is_loopback()` | `Host::Ipv6(ip)` where
      `ip.is_loopback()`. `None` host → `MissingHost`. Never resolve DNS; never suffix-match.
- [ ] Implement `join_url`: `format!("{}{}", base_url.trim_end_matches('/'), path_suffix)`.
- [ ] Add `mod base_url;` and `pub use base_url::{BaseUrlError, validate_base_url};` to
      `crates/providers/src/lib.rs`.
- [ ] Run `cargo test -p otto-providers base_url` — expect all green.
- [ ] Format and commit: `cargo fmt --all` then
      `git commit -m "providers: add base-url validation and slash-safe join"`.

### Task 2: Compose the endpoint with `join_url` (#112)

**Files:** `crates/providers/src/openai_compatible.rs`

**Interfaces:** consumes `join_url` from Task 1. Produces no new public surface.

- [ ] Add a failing test to `openai_compatible.rs`'s `mod tests` asserting the endpoint composition
      for both suffixes and a trailing-slash base. Since `complete` is async and network-bound,
      test composition through `join_url` directly rather than by standing up a server — the
      wiremock tests in `openai.rs`/`deepseek.rs` already cover the live path.
- [ ] Add a wiremock regression test (in `openai.rs`) that constructs `OpenAiProvider::new` with a
      base URL carrying an explicit trailing slash — `format!("{}/", server.uri())` — and asserts
      the request still matches `path("/v1/chat/completions")`. This is the real proof of #112:
      before the fix the server would see `//v1/chat/completions` and the mock would not match.
- [ ] Run `cargo test -p otto-providers` — expect the new trailing-slash test to FAIL.
- [ ] Replace `let url = format!("{}{}", self.base_url, self.path_suffix);` at
      `openai_compatible.rs:51` with `let url = super::base_url::join_url(&self.base_url, self.path_suffix);`
      (import as appropriate).
- [ ] Run `cargo test -p otto-providers` — expect all green, including the pre-existing 25.
- [ ] Format and commit: `cargo fmt --all` then
      `git commit -m "providers: compose the chat endpoint without a doubled slash"`.

### Task 3: Validate at the env trust boundary (#111) + docs

**Files:** `crates/engine/src/lib.rs`, `CLAUDE.md`

**Interfaces:** consumes `otto_providers::validate_base_url`. Changes `build_remote`'s signature
(private to the crate).

- [ ] Write the failing test first, in `crates/engine/src/lib.rs`'s `mod tests`: call
      `resolve_base_url(Some("http://evil.example.com".into()), "https://api.openai.com", "OPENAI_BASE_URL")`
      and assert `None`; assert `Some("http://127.0.0.1:9/".into())`-style loopback input is
      returned as `Some`; assert `None` override returns `Some(default)`.
      **This test must not touch `std::env`** — pass values as arguments only.
- [ ] Run `cargo test -p otto-engine --lib resolve_base_url` — expect compile failure.
- [ ] Implement `fn resolve_base_url(override_value: Option<String>, default: &str, var_name: &str) -> Option<String>`:
      `None` override → `Some(default.to_string())`; `Some(v)` → `validate_base_url(&v)`, on `Ok`
      return `Some(v)`, on `Err(e)` `eprintln!` a warning naming `var_name`, the rejected value and
      the reason, stating the engine is falling back to the offline router, then return `None`.
      **The warning must never print an API key.**
- [ ] Change `build_remote` to `-> Option<Arc<dyn Provider>>`. Anthropic and Gemini arms wrap in
      `Some(...)` unchanged (they have no env override). The OpenAI and DeepSeek arms read
      `std::env::var("…_BASE_URL").ok()`, pass it through `resolve_base_url(…)?`, and construct the
      provider only on `Some`.
- [ ] Update both `build_router_with_model` call sites to handle `None`: the `Some(model)` arm's
      `build_remote(...)` becomes a `match`/`and_then` that falls back to
      `SingleProviderRouter::new(local)`; the `None` arm likewise. Do **not** add a second warning
      in either arm — `resolve_base_url` already printed the specific one.
- [ ] Run `cargo test -p otto-engine --lib` — expect all green, including the pre-existing 74 and
      the two env-removing tests, which must remain the only env-touching tests in the binary.
- [ ] Update `CLAUDE.md`'s "Runtime configuration (env vars)" section: for both `OPENAI_BASE_URL`
      and `DEEPSEEK_BASE_URL`, state that the override must be `https://`, that plain `http://` is
      accepted only for loopback hosts, and that an invalid value falls back to the offline router
      with a warning rather than sending the key.
- [ ] Run the full gate: `cargo fmt --all --check`,
      `cargo clippy --workspace --all-targets -- -D warnings`, and
      `cargo test --workspace -- --skip rust_analyzer_integration`. All must pass.
- [ ] Format and commit: `cargo fmt --all` then
      `git commit -m "engine: validate provider base URLs before sending the API key"`.
