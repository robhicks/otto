# Provider base-URL hardening: scheme validation and slash-safe composition

> **Status:** DRAFT — fixes [#111](https://github.com/robhicks/otto/issues/111) and
> [#112](https://github.com/robhicks/otto/issues/112).
> **Found by:** the independent security review on [#108](https://github.com/robhicks/otto/pull/108)
> (both filed as Minor and deliberately deferred out of that PR).

`OpenAiProvider` and `DeepSeekProvider` share one wire implementation
(`crates/providers/src/openai_compatible.rs`). Two defects in how that implementation treats its
`base_url` were left open by #108. Both are config-robustness issues in the same few lines, so they
are fixed together.

---

## Why one spec for two issues

#111 (scheme validation) and #112 (slash composition) are both "how the OpenAI-compatible provider
derives its endpoint from operator-supplied config", they touch the same function
(`OpenAiCompatibleProvider::complete`) and the same env reads (`build_remote`), and they came from
the same review pass. Shipping them as two PRs would mean two changes racing on the same file for
no benefit. **Assumption:** a single spec/plan/PR closing both issues is preferred over serialized
PRs; each issue's acceptance criteria are verified independently by their own tests, so nothing is
lost.

## The defects

### #111 — the API key goes to whatever host the env names, over whatever scheme

`crates/engine/src/lib.rs:288` and `:302` read `OPENAI_BASE_URL` / `DEEPSEEK_BASE_URL` and pass the
value straight through to the provider. `OpenAiCompatibleProvider::complete`
(`openai_compatible.rs:62-66`) then does:

```rust
.post(&url)
.header("authorization", format!("Bearer {}", self.api_key))
```

There is no scheme or host check anywhere on that path. An `http://` base — mistyped, injected by a
wrapper script, or set by a poisoned environment — ships the API key in cleartext. Any host the env
names receives the key verbatim.

**Bounding it honestly:** the defaults are correct (`https://api.openai.com`,
`https://api.deepseek.com`), the value comes from the operator's own environment, and nothing
agent-, model-, or prompt-derived reaches it. This is hardening against operator error and
environment poisoning, **not a live exploit**. `ANTHROPIC_API_KEY` and Gemini use fixed endpoints
(`api_base_default()` with no env override) and are unaffected — until someone adds a
`*_BASE_URL` override for them, at which point they must adopt the same validation.

### #112 — naive concatenation doubles the separator

`openai_compatible.rs:51`:

```rust
let url = format!("{}{}", self.base_url, self.path_suffix);
```

A base ending in `/` (`https://api.example.com/v1/`, or a pasted value with a trailing slash) yields
`https://host//v1/chat/completions`. Most servers tolerate the doubled slash; some proxies and
gateways do not, and it would break outright for any future provider whose `path_suffix` is a full
path rather than a suffix. Not exploitable — `base_url` is env config and `path_suffix` is a
`&'static str`, so no agent/model/prompt data reaches the URL.

---

## Scope

**In:**
- A shared, unit-testable base-URL module in `otto-providers`: scheme/host validation + slash-safe
  join.
- `build_remote` validates the resolved base URL **before** constructing the provider, for both
  `OPENAI_BASE_URL` and `DEEPSEEK_BASE_URL`.
- `OpenAiCompatibleProvider::complete` composes the endpoint without doubled separators.
- Tests for both, including the loopback carve-out that keeps wiremock working.
- A `CLAUDE.md` update: the "Runtime configuration (env vars)" section documents
  `OPENAI_BASE_URL`/`DEEPSEEK_BASE_URL` as plain endpoint overrides. Since this is a user-visible
  behavior change with no escape hatch, that section must state the https-or-loopback constraint.

**Out:**
- Changing `OpenAiProvider::new` / `DeepSeekProvider::new` to return `Result`. See §2.
- Validating Anthropic/Gemini/Ollama bases (no env override exists for them today).
- Full RFC-3986 normalization, redirect policy, certificate pinning, or proxy handling.
- Any change to the request body, headers, auth scheme, or response parsing.

---

## §1 — The validation rule

New module `crates/providers/src/base_url.rs`, public from the crate root.

```rust
pub fn validate_base_url(base_url: &str) -> Result<(), BaseUrlError>
```

Accept:
- any `https://` URL — the production case;
- `http://` **only** when the host is loopback: the domain `localhost`, an IPv4 address for which
  `is_loopback()` holds (`127.0.0.0/8`), or IPv6 `::1`.

Reject, with a distinct error variant each:
- `Unparseable` — the string is not a URL at all;
- `UnsupportedScheme` — any scheme that is not `http`/`https` (`ftp`, `file`, `ws`);
- `InsecureScheme` — `http://` to a non-loopback host — **the security case**;
- `MissingHost` — a URL with no host at all.

**On `MissingHost` being defensive-only.** For the `http`/`https` schemes the `url` crate
guarantees a non-empty host: `https://` and `http://` fail parsing outright with `EmptyHost` (so
they land in `Unparseable`), and `https:///foo` does **not** mean "no host" — it parses with host
`Domain("foo")` and is therefore *accepted*. The only genuinely hostless inputs (`file:///x`) are
caught by the scheme check first. So `MissingHost` is unreachable in practice; it exists so the
`host()` match is total rather than an `unwrap`. **Do not write a test asserting some input
produces `MissingHost` — there is none.**

Host comparison is exact equality against the parsed host, never a suffix or substring match, and
never a DNS resolution. `url` lowercases the host during parsing, so the `localhost` check is
case-insensitive for free.

**Why loopback-`http` is allowed:** every provider test constructs against `server.uri()` from
wiremock, which is `http://127.0.0.1:<port>`. Rejecting plain `http` outright would break the
existing 25 `otto-providers` tests and make the provider untestable without TLS. Loopback traffic
does not leave the machine, so a key sent there is not exposed to the network. This is the carve-out
#111 explicitly asks for.

**Why a domain allowlist of exactly `localhost`:** an attacker-controlled DNS name that *resolves*
to 127.0.0.1 (the classic SSRF rebind) would still be sent the key if we resolved names. We do not
resolve — we match the literal host — so `http://evil.example.com` is rejected regardless of what
it resolves to. Conversely `http://localhost.evil.com` does not match, because the check is
equality, not suffix.

`BaseUrlError` is a `thiserror`-free plain enum implementing `Display` **and a four-line empty
`impl std::error::Error`** (still no new dep), so the public error type composes with `anyhow`/`?`
for future callers. Its `Display` names the offending URL and says what was expected, so the
operator can see which env var to fix. **The API key is never included in any error string** — the
key is never in scope in this module.

## §2 — Where validation runs, and how it fails

**Validation runs in `build_remote` (`crates/engine/src/lib.rs`), not in the provider
constructors.**

Rationale: the trust boundary is the *env read*, not the constructor. `OpenAiProvider::new` is a
library API whose callers pass explicit values they already control (every current caller is a test
passing `server.uri()`). CLAUDE.md's determinism convention puts every `OTTO_*`/API-key/env read
behind `build_router` — the base-URL read lives there too, so its validation belongs there. This
also avoids turning two public constructors into `Result`-returning ones and rippling `.unwrap()`
through ~12 test call sites for no safety gain.

**Failure mode: warn loudly and fall back to the offline router.**

`build_remote` changes signature from `-> Arc<dyn Provider>` to `-> Option<Arc<dyn Provider>>`,
returning `None` when validation fails. Both call sites in `build_router_with_model` fall back to
`SingleProviderRouter(local)` on `None`.

**Correction to an earlier draft of this spec:** only the `Some(model)` arm has an existing warning
(`crates/engine/src/lib.rs:346-349`). The `None` arm — the branch every ordinary `otto run` /
`otto serve` takes — falls through to `SingleProviderRouter(local)` at `lib.rs:359` with **no
output at all**. Reusing that arm as-is would make a rejected base URL a *silent* downgrade,
contradicting #111's "fail with a clear error message". The warning therefore must not live in the
router arms.

**The warning is emitted by the resolver itself** (see below), so it fires on both branches by
construction and neither arm needs new printing code.

Degrading to offline is strictly safer than the alternatives:

| Alternative | Why not |
|---|---|
| Send to the default `https://` base instead | Silently ignores operator config while still transmitting the key. The operator believes they retargeted the provider; they did not. |
| `panic!` / `process::exit` | `build_router` is a library function called from several entrypoints; killing the process from inside it is not its business. |
| Make `build_router` return `Result` | Ripples through every caller (`run_goal`, `EngineService`, `cmd_run`, `cmd_serve`, `run_command_in`, `run_custom_agent_in`) for one config check. Out of proportion. |

The warning goes to stderr, names the env var and the rejected value, and states that the engine is
falling back to offline. **It must not print the API key.**

### §2a — A pure resolver seam, so this is testable without mutating the environment

`crates/engine/src/lib.rs:596-601` carries an explicit SAFETY contract on the two env-touching
tests in this test binary: both only ever *remove* provider-selection vars, "so ordering is
irrelevant and they cannot race destructively", and it states verbatim: **"Do not add a test here
that SETS these vars without revisiting this comment."**

Proving #111's engine-level behavior by setting `OPENAI_API_KEY` + `OPENAI_BASE_URL` inside a test
would violate that contract and introduce a real data race under edition 2024's
`unsafe { set_var }`, run in parallel with the sibling tests. So the env read is separated from the
decision:

```rust
/// Resolve a provider's base URL from an optional env override.
/// `Some(url)` to use; `None` when an override was present but rejected — the caller
/// falls back to the offline router. Warns to stderr on rejection.
fn resolve_base_url(override_value: Option<String>, default: &str, var_name: &str) -> Option<String>
```

`resolve_base_url` is **pure with respect to the environment** — the caller passes the value in.
`build_remote` does `std::env::var("OPENAI_BASE_URL").ok()` and hands it over. The engine-level
tests call `resolve_base_url` directly with literal `Some("http://evil.example.com".into())` and
assert `None`, touching no environment variable, so the SAFETY comment stays true as written and no
new serialization or mutex is needed.

**Assumption:** covering #111's engine AC through this pure seam plus the exhaustive
`validate_base_url` unit tests in `otto-providers` satisfies "a non-loopback `http://` base is
rejected" — the issue explicitly permits "`build_router` **or** provider-construction unit test".
No test in this repo will set a provider env var.

**Determinism is preserved:** with no env vars set, neither `OPENAI_BASE_URL` nor `DEEPSEEK_BASE_URL`
is present, `build_remote` is not reached (no key ⇒ no remote selected), and both router slots stay
`LocalProvider`. The offline default is byte-for-byte unchanged.

## §3 — Slash-safe composition

Same module:

```rust
pub(crate) fn join_url(base_url: &str, path_suffix: &str) -> String
```

Trim **all** trailing `/` from `base_url`, then concatenate with `path_suffix` (which is always a
`&'static str` beginning with `/`). `complete` calls it in place of the `format!`.

Trimming repeated slashes (`https://host///`) rather than exactly one is deliberate: it costs
nothing and makes the function total over pasted input. The base's own path prefix is preserved —
`https://host/v1` + `/chat/completions` → `https://host/v1/chat/completions` — so Azure-style bases
keep working.

`join_url` does no parsing and cannot fail; validation (§1) has already run at the trust boundary.
Keeping them separate means the pure string function stays trivially testable across the matrix
#112 asks for.

---

## Goal & Success Criteria

Make the OpenAI-compatible providers robust to operator-supplied base URLs: never transmit an API
key over cleartext to a non-loopback host, and never emit a malformed endpoint.

- `validate_base_url` accepts `https://…` and loopback `http://…`; rejects non-loopback `http://`,
  non-HTTP schemes, hostless URLs, and unparseable input.
- A non-loopback `http://` base in `OPENAI_BASE_URL` or `DEEPSEEK_BASE_URL` results in **no
  provider constructed, no request sent, and no key transmitted** — the engine falls back to the
  offline router with a stderr warning.
- `join_url` produces exactly one separator for bases with zero, one, or many trailing slashes, and
  preserves a path prefix, across both `/v1/chat/completions` and `/chat/completions`.
- `cargo test -p otto-providers` and `cargo test -p otto-engine --lib` stay green (the pre-existing
  counts were 25 and 74; new tests add to them).
- The offline-deterministic default path is unchanged.
- **No test added by this change sets or removes a process environment variable**, so the SAFETY
  contract at `crates/engine/src/lib.rs:596-601` remains true verbatim.

## Error Handling & Edge Cases

| Input | Result |
|---|---|
| `https://api.openai.com` | accept |
| `https://api.openai.com/v1/` | accept; join trims to one slash |
| `http://127.0.0.1:8080` (wiremock) | accept — loopback |
| `http://localhost:1234` | accept — loopback |
| `http://[::1]:8080` | accept — loopback |
| `http://api.openai.com` | **reject** — cleartext to a public host |
| `http://localhost.evil.com` | **reject** — equality, not suffix match |
| `http://169.254.169.254` (cloud metadata) | **reject** — not loopback |
| `http://2130706433` | **accept** — parses to `Ipv4(127.0.0.1)`, i.e. genuinely loopback |
| `http://localhost.` | **reject** — trailing dot parses as `Domain("localhost.")`, not equal to `localhost` |
| `http://LOCALHOST` | accept — `url` lowercases the host during parsing |
| `ftp://host` / `file:///etc/passwd` | reject — scheme not HTTP(S) (`UnsupportedScheme`) |
| `not a url` / `""` | reject — `Unparseable` |
| `https://` / `http://` | reject — `url` fails these with `EmptyHost`, so they land in `Unparseable` |
| `https:///foo` | **accept** — this is NOT hostless; it parses as host `Domain("foo")` |

An empty-string env var (`OPENAI_BASE_URL=`) is *present but unparseable*, so it is rejected rather
than treated as unset. **Assumption:** this is the desired behavior — an explicitly-empty override
is operator error worth surfacing, not a silent request for the default. Documented here because it
is a small behavior change for anyone exporting an empty var.

## Risks & Open Questions

- **Behavior change for existing users.** Anyone currently pointing `OPENAI_BASE_URL` at a
  non-loopback `http://` endpoint (a LAN LLM gateway, say) loses their remote and drops to offline
  with a warning. That is the point of the issue, but it is a breaking change for that
  configuration. There is deliberately **no escape hatch env var** — adding one would recreate the
  hole it closes. Such users should terminate TLS in front of the gateway or tunnel it to loopback.
  Called out in the PR body so it is not a surprise.
- **Non-loopback private ranges** (`10.0.0.0/8`, `192.168.0.0/16`, `.internal` names) are rejected.
  A stricter-than-necessary rule was chosen over an ambiguous "private is fine" rule; private
  networks are not trustworthy transport for a bearer credential.
- `reqwest::Url` (a re-export of the `url` crate) is used for parsing, so **no new Cargo dependency
  is added** — `reqwest` is already a direct dependency of `otto-providers`.
