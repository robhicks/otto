# Design: `openai` + `gemini` providers

**Date:** 2026-07-08
**Status:** Approved (design)

## Summary

Build the two named-but-missing `Provider` impls — `OpenAiProvider` and `GeminiProvider` —
in the `providers` crate, and teach the engine's router-wiring layer to select among three
remote providers instead of hardwiring the remote slot to Anthropic. This unlocks real
cross-provider model choice (via `OTTO_REMOTE_PROVIDER`, per-provider keys, and portable
`--model` ids) while preserving the byte-for-byte offline-deterministic default (no env vars
set → both router slots are `LocalProvider`).

## Motivation

`crates/providers/` names `anthropic`, `gemini`, `openai`, `local(ollama)` in the architecture,
but only Anthropic (plus Local/Scripted/Ollama) exists. `build_router_with_model` in
`crates/engine/src/lib.rs` constructs an `AnthropicProvider` for the remote slot unconditionally
when `ANTHROPIC_API_KEY` is present — there is no way to route to another remote. Filling this in
is self-contained (no new infra), high user value, and fully testable offline with `wiremock`,
mirroring the existing `AnthropicProvider` test pattern.

## Non-goals (YAGNI)

- **No multi-remote pool.** The router keeps a single remote slot (one local + one remote `Arc`),
  as `BrainBlendRouter`/`PinnedModelRouter` already expect. Selection picks exactly one remote.
- **No streaming, tool-use, or multi-turn message history.** `CompleteRequest` carries only a
  `prompt`; the new providers send a single user message, exactly as `AnthropicProvider` does.
- **No o-series `max_completion_tokens` special-casing.** v1 sends a fixed construction-time
  `max_tokens`/`maxOutputTokens` like Anthropic; the o-series `max_completion_tokens` quirk is
  noted below but deferred.
- **No `gemini`/`openai` addition to the `Provisioner`/remote axis.** This is purely the
  LLM-provider axis.

## Design

### 1. Provider impls (`crates/providers/`)

Each new file mirrors `anthropic.rs` one-for-one: a struct holding
`client: reqwest::Client`, `base_url: String`, `api_key: String`, `model: String`, and a
construction-time `max_tokens: u32` (matching `AnthropicProvider`'s field); a
`new(base_url, api_key, model)` constructor and an `api_base_default() -> &'static str`; a
configurable `base_url` so `wiremock` can point at a local mock; non-2xx surfaced as an
`anyhow` error via `reqwest`'s `error_for_status()`; and `usage` mapped into
`otto_engine_core::types::Usage` when the response reports it (`None` otherwise).

No new Cargo dependencies — `reqwest` (json + rustls-tls), `serde`, `serde_json`, and the
`wiremock` dev-dependency are already present in `providers/Cargo.toml`.

#### `OpenAiProvider` (`crates/providers/src/openai.rs`)

- `id() -> "openai"`.
- `api_base_default() -> "https://api.openai.com"`.
- `complete()`: `POST {base_url}/v1/chat/completions` with header
  `Authorization: Bearer {api_key}`, body:
  ```json
  { "model": "<model>", "max_tokens": <n>,
    "messages": [ { "role": "user", "content": "<prompt>" } ] }
  ```
- Response parse: text from `choices[0].message.content`; usage from
  `usage.prompt_tokens` → `input_tokens`, `usage.completion_tokens` → `output_tokens`.
- Uses `#[serde(default)]` on optional fields (mirroring `anthropic.rs`) so a minimal mock
  response deserializes.
- **Caveat (documented, deferred):** OpenAI o-series models reject `max_tokens` in favor of
  `max_completion_tokens`. v1 targets the `gpt-*` chat-completions models (the default is
  `gpt-4o-mini`); o-series support is out of scope.

#### `GeminiProvider` (`crates/providers/src/gemini.rs`)

- `id() -> "gemini"`.
- `api_base_default() -> "https://generativelanguage.googleapis.com"`.
- `complete()`: `POST {base_url}/v1beta/models/{model}:generateContent` with header
  `x-goog-api-key: {api_key}` (header form, not the `?key=` query param — cleaner and
  wiremock-friendly), body:
  ```json
  { "contents": [ { "role": "user", "parts": [ { "text": "<prompt>" } ] } ],
    "generationConfig": { "maxOutputTokens": <n> } }
  ```
- Response parse: text from `candidates[0].content.parts[0].text` (joined across parts, as
  Anthropic joins content blocks); usage from `usageMetadata.promptTokenCount` →
  `input_tokens`, `usageMetadata.candidatesTokenCount` → `output_tokens`.

#### `lib.rs`

Add `pub use openai::OpenAiProvider;` and `pub use gemini::GeminiProvider;` alongside the
existing exports.

### 2. Router wiring (`crates/engine/src/lib.rs`)

The remote-slot construction is generalized behind two **pure, `pub(crate)`, unit-testable**
helpers plus a builder. `build_local_provider()` (the `OTTO_OLLAMA`/`LocalProvider` selection)
is unchanged.

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemoteChoice { Anthropic, OpenAi, Gemini }
```

**Default (non-pinned) selection:**

```
select_remote() -> Option<RemoteChoice>
  1. If OTTO_REMOTE_PROVIDER is set to a known value ("anthropic" | "openai" | "gemini"):
       - if that provider's API key is present & non-empty -> Some(choice)
       - else warn ("OTTO_REMOTE_PROVIDER=<x> but <KEY> unset") -> None (offline)
     If set to an unknown value -> warn, ignore, fall through to precedence.
  2. Precedence over present non-empty keys:
       ANTHROPIC_API_KEY > OPENAI_API_KEY > GEMINI_API_KEY  (first wins)
  3. No key present -> None
```

This preserves today's behavior exactly: with only `ANTHROPIC_API_KEY` set and no
`OTTO_REMOTE_PROVIDER`, `select_remote()` returns `Anthropic`.

**Pinned selection (model-id inference):**

```
infer_remote(model_id) -> Option<RemoteChoice>
  gpt-* | o1* | o3* | o4*  -> OpenAi
  gemini-*                 -> Gemini
  claude-*                 -> Anthropic
  otherwise                -> None
```

**Provider construction:**

```
build_remote(choice, model) -> Arc<dyn Provider>
  Anthropic -> AnthropicProvider::new(api_base_default, ANTHROPIC_API_KEY, model)
  OpenAi    -> OpenAiProvider::new(OPENAI_BASE_URL or api_base_default, OPENAI_API_KEY, model)
  Gemini    -> GeminiProvider::new(api_base_default, GEMINI_API_KEY, model)
```

`OPENAI_BASE_URL` (when set) overrides the default base, enabling Azure/OpenAI-compatible
endpoints. `build_remote` reads the key for the given `choice`; callers only invoke it after a
key has been confirmed present.

**`build_router_with_model(model_override: Option<&str>)` rewrite:**

- `let local = build_local_provider();`
- **`None`** (default path):
  - `match select_remote()`:
    - `Some(choice)` → `remote = build_remote(choice, default_model_for(choice))` →
      `Box::new(BrainBlendRouter::new(local, remote))`
    - `None` → `Box::new(SingleProviderRouter::new(local))` — **unchanged deterministic default**
- **`Some(model)`** (pinned path):
  - `let choice = infer_remote(model).or_else(select_remote);`
  - `match choice`:
    - `Some(c)` **and** `c`'s key present → `remote = build_remote(c, model.to_string())` →
      `Box::new(PinnedModelRouter::new(local, remote))`
    - otherwise → warn ("requested model '<m>' but no usable provider key") →
      `Box::new(SingleProviderRouter::new(local))` (mirrors the current no-key fallback,
      keeping the deterministic default)

`default_model_for(choice)` reads the per-provider `OTTO_<P>_MODEL` env var, falling back to a
constant:

- `DEFAULT_ANTHROPIC_MODEL` — existing (`claude-haiku-4-5`).
- `DEFAULT_OPENAI_MODEL = "gpt-4o-mini"`.
- `DEFAULT_GEMINI_MODEL = "gemini-2.5-flash"`.

(Cheap-tier defaults, matching the existing Anthropic default's intent.)

Privacy-sensitive routing is unaffected: `BrainBlendRouter` and `PinnedModelRouter` route
privacy-sensitive requests to the local slot and are provider-agnostic — neither changes.

### 3. Capability probe + `session_config`

- The `remote_llm` capability's truth source widens from "`ANTHROPIC_API_KEY` present &
  non-empty" to `select_remote().is_some()`, so a session served with only `OPENAI_API_KEY`
  (or `GEMINI_API_KEY`, or a valid `OTTO_REMOTE_PROVIDER`) correctly reports a remote LLM.
- `session_config` records the **resolved** remote provider id and model — a `remote_provider`
  field (the selected choice's id, or `"none"`) and a `remote_model` field — generalizing the
  current `anthropic_model` record. `ollama_model` recording is unchanged.

These are the two touch points that currently special-case Anthropic; both must move to the
`select_remote()` seam so they stay consistent with routing.

## Error handling

- Non-2xx HTTP → `anyhow::Error` carrying the status (via `error_for_status()`), identical to
  `AnthropicProvider`. The router's existing cross-provider fallback (`BrainBlendRouter`) already
  falls back to local on a non-privacy remote error for liveness — unchanged and now covers the
  new providers.
- A selector naming a provider whose key is absent is a configuration error surfaced as a
  `warning:` to stderr and treated as "no remote" (offline fallback), never a panic.
- Malformed/short provider responses deserialize via `#[serde(default)]`; an entirely
  unparseable body surfaces as the underlying `reqwest`/`serde` error through `?`.

## Testing

Tests live next to code (`#[cfg(test)] mod tests`), matching repo convention.

**Per provider (`openai.rs`, `gemini.rs`) — `wiremock`, cloned from `anthropic.rs`'s three:**
1. Posts the correct path + auth header + JSON body and parses the response text.
2. Parses usage tokens into `Usage`.
3. Surfaces an HTTP error (non-2xx) as an `Err`.

**Router (`engine/src/lib.rs`) — pure-function unit tests over the new seams:**
- `select_remote()`: `OTTO_REMOTE_PROVIDER` selector wins when its key is present; precedence
  (Anthropic > OpenAi > Gemini) when the selector is unset; selector-without-key → `None`;
  unknown selector value → precedence fallback; no keys → `None`.
- `infer_remote()`: `gpt-4o`/`o3-mini` → `OpenAi`, `gemini-2.5-pro` → `Gemini`,
  `claude-opus-4-8` → `Anthropic`, unknown → `None`.
- **Determinism invariant (critical):** no keys + no `OTTO_REMOTE_PROVIDER` → offline &
  deterministic router (reuse the existing serialized-env `_ENV_LOCK` pattern that
  `model_override_without_key_is_offline_and_deterministic` uses; save/remove/restore
  `OTTO_OLLAMA`, `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GEMINI_API_KEY`, `OTTO_REMOTE_PROVIDER`).

Provider *selection* is asserted against the pure helpers (no network), not against a live
completion — mirroring how the existing router tests assert routing without hitting a provider.

## Files touched

| File | Change |
|---|---|
| `crates/providers/src/openai.rs` | **new** — `OpenAiProvider` + 3 wiremock tests |
| `crates/providers/src/gemini.rs` | **new** — `GeminiProvider` + 3 wiremock tests |
| `crates/providers/src/lib.rs` | export both new providers |
| `crates/engine/src/lib.rs` | `RemoteChoice`, `select_remote`, `infer_remote`, `build_remote`, `default_model_for`, `DEFAULT_OPENAI_MODEL`/`DEFAULT_GEMINI_MODEL`; rewrite `build_router_with_model`; widen the `remote_llm` predicate + generalize `session_config`'s remote record; new unit tests |

Two files added, two edited. No new dependencies. The only wire-visible change is widening the
source of `CapabilitiesManifest::remote_llm` (already a bool field — no schema change) and the
internal `session_config` record.

## Determinism guarantee

With no `OTTO_OLLAMA`, no provider keys, and no `OTTO_REMOTE_PROVIDER`, `select_remote()`
returns `None` and `build_router_with_model(_)` yields `SingleProviderRouter::new(LocalProvider)`
— identical to today. The offline determinism suite and CI (which set none of these) are
untouched.
