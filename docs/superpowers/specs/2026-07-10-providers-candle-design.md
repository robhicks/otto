# Design: in-process `candle` provider

**Date:** 2026-07-10
**Status:** Approved (design)

## Summary

Add a new `Provider` impl — `CandleProvider` — to the `providers` crate that runs a small
quantized **Gemma 3** (GGUF) model **in-process** via [candle](https://github.com/huggingface/candle),
with no external daemon and (in local-file mode) no network I/O at all. It fills the engine's
**local router slot** — an alternative to `LocalProvider` (deterministic stub) and
`OllamaProvider` (HTTP to a separate daemon) — selected by `OTTO_CANDLE=1`. The whole thing sits
behind a default-off `candle` cargo feature, so `cargo build --workspace` and the offline
determinism suite are byte-for-byte unchanged.

## Motivation

The only real local inference today is `OllamaProvider`, which requires a separately-installed,
separately-running Ollama daemon reachable over HTTP. An in-process candle provider gives us:

- **A single self-contained binary** — otto runs the model itself; nothing to install or start
  alongside it (valuable for the microVM/remote targets, where provisioning a sidecar daemon is
  friction).
- **Privacy / air-gap** — in local-file mode the provider makes *no* network calls, at load time
  or inference time; the model bytes never leave the process.
- **Direct control** — quantization, device (CPU/CUDA/Metal), sampling, and generation length are
  ours to set, with no Ollama layer in between.

candle's `quantized-gemma` example and `models::quantized_gemma3` module already support loading
Gemma 3 GGUF from **local files** (not just Hub auto-download) and CPU inference with SIMD, so this
is a wiring exercise against an existing, maintained model implementation — no model code to write.

## Non-goals (YAGNI)

- **No embeddings / retrieval integration.** candle-based semantic embeddings for the `retrieval`
  crate is a *separate follow-on sub-project* with its own spec, seam, and storage concerns. This
  spec is the completion `Provider` only.
- **No backend-abstraction layer.** One `CandleProvider` wired directly to `quantized_gemma3`. No
  `CandleModel` trait / multi-architecture indirection — that is speculative until a second
  architecture or embeddings actually lands, and is a trivial internal refactor when it does.
- **No multi-architecture support.** Gemma 3 only. Llama/Phi/Gemma-switching is out.
- **No streaming.** The `Provider` seam is non-streaming (`complete()` returns a full
  `CompleteResponse`); candle generates then returns.
- **No training / fine-tuning.** Inference only.
- **No change to the default deterministic path.** With the `candle` feature off (the default) and
  `OTTO_CANDLE` unset, nothing here compiles into or affects the offline suite.
- **No `Provider`/`Router`/`CompleteRequest`/`CompleteResponse` trait or type changes.** It drops
  into the existing seam unchanged.

## Design

### 1. Crate placement & feature gating (`crates/providers/`)

- New file `crates/providers/src/candle.rs` defining `CandleProvider`. `lib.rs` declares
  `mod candle;` / `pub use candle::CandleProvider;` **only under `#[cfg(feature = "candle")]`**.
- `providers/Cargo.toml` gains **optional** dependencies: `candle-core`, `candle-transformers`,
  `tokenizers`, `hf-hub`. New features:
  - `candle` = enables the four deps + the module. Default features stay empty.
  - `candle-cuda` = `candle` + `candle-core/cuda` + `candle-transformers/cuda`.
  - `candle-metal` = `candle` + `candle-core/metal` + `candle-transformers/metal`.
- The `engine` crate gets a passthrough `candle` feature (`providers/candle`), plus
  `candle-cuda` / `candle-metal` passthroughs, so the binary can opt in.
- **Invariant:** with default features, `providers` and `engine` build and test exactly as today —
  none of the candle deps are pulled, `CandleProvider` does not exist.

### 2. Model loading & config

Configuration is read in the engine's router-wiring layer (`build_router` /
`build_router_with_model` in `crates/engine/src/lib.rs`) — **never in core** — from env vars,
mirroring how `OTTO_OLLAMA` is handled:

- `OTTO_CANDLE=1` — opt the candle provider into the **local router slot** (replaces the
  `LocalProvider`/`OllamaProvider` that slot would otherwise hold).
- `OTTO_CANDLE_MODEL` — model source:
  - If it names an **existing `.gguf` file on disk**, load it directly → **zero network I/O**
    (true air-gap).
  - Otherwise treat it as a **HuggingFace repo id** and resolve the GGUF via `hf-hub` into the OS
    cache dir (network at load time only, never at inference).
  - **Default** when unset: a small Gemma 3 instruct QAT GGUF repo id
    (e.g. `google/gemma-3-1b-it-qat-q4_0-gguf`).
- **Tokenizer:** `tokenizer.json` loaded via the `tokenizers` crate from the model file's sibling
  directory (local-file mode) or the same hf-hub repo (download mode).
- **Device:** `Device::Cpu` by default. When built with `candle-cuda` / `candle-metal`, prefer the
  accelerator if available, falling back to CPU otherwise.
- `id()` returns `"candle"`.

Model load happens **once**, at provider construction (in `build_router`), not per `complete()`
call — the loaded model + tokenizer live in the `CandleProvider` (behind the `Send + Sync`
requirement; interior mutability as needed for the generation KV-cache/state).

### 3. Generation (`complete()`)

- **Prompt template:** by default wrap `req.prompt` in the Gemma instruct turn format
  (`<start_of_turn>user\n{prompt}<end_of_turn>\n<start_of_turn>model\n`), since the default model is
  instruct-tuned. `OTTO_CANDLE_RAW=1` disables wrapping (for base/non-instruct models).
- **Sampling:** greedy (argmax) by default → **reproducible** output for a given model + prompt.
  `OTTO_CANDLE_TEMPERATURE` and `OTTO_CANDLE_TOP_P` opt into stochastic sampling via candle's
  `LogitsProcessor`.
- **Length / stop:** `OTTO_CANDLE_MAX_TOKENS` (default e.g. `512`) caps generated tokens; generation
  also stops on Gemma's `<end_of_turn>` / EOS token.
- **Usage:** `CompleteResponse.usage` is filled from actual prompt-token and generated-token counts
  (`Usage { input_tokens, output_tokens }`).
- Errors (model/tokenizer load failure, decode failure) surface as `anyhow::Error` from
  `complete()` — consistent with the other providers.

### 4. Router wiring (`crates/engine/src/lib.rs`)

- Under `#[cfg(feature = "candle")]`, `build_router` / `build_router_with_model` check
  `OTTO_CANDLE=1` and, when set, construct `CandleProvider` for the local slot instead of
  `LocalProvider` / `OllamaProvider`.
- **Precedence:** if both `OTTO_CANDLE` and `OTTO_OLLAMA` are set, **candle wins** the local slot,
  with a one-line warning. Remote-slot selection (Anthropic/OpenAI/Gemini precedence) is untouched.
- With the `candle` feature **off**, this branch does not compile in and the local slot behaves
  exactly as today.

### 5. Testing

`cargo test --workspace` (default features) is untouched — the candle module isn't compiled and no
model ever loads.

Under `--features candle`, these unit tests run **without a model forward pass** (pure logic over
small fixtures, CI-friendly):

- **Prompt templating** — Gemma turn-format wrapping, and the `OTTO_CANDLE_RAW` bypass.
- **Sampling** — argmax and temperature/top-p selection over a small synthetic logits `Tensor`.
- **Env-config parsing** — model source (path vs repo id) resolution, max-tokens/temperature parse
  and defaults.
- **Tokenizer round-trip** — encode/decode against a tiny checked-in `tokenizer.json` fixture.

One **end-to-end inference test** is gated behind `feature = "candle"` **and** `#[ignore]` (it needs
a real model supplied via `OTTO_CANDLE_MODEL`); it is run manually/opt-in and never in the default
suite.

## Affected files

- `crates/providers/Cargo.toml` — optional candle deps + `candle`/`candle-cuda`/`candle-metal`
  features.
- `crates/providers/src/candle.rs` — new: `CandleProvider`, generation, unit tests.
- `crates/providers/src/lib.rs` — feature-gated `mod`/`pub use`.
- `crates/engine/Cargo.toml` — passthrough `candle`/`candle-cuda`/`candle-metal` features.
- `crates/engine/src/lib.rs` — `OTTO_CANDLE` selection in `build_router` / `build_router_with_model`
  (feature-gated).
- `tests/fixtures/` (or crate-local) — tiny `tokenizer.json` fixture for the round-trip test.
- `CLAUDE.md` — provider table + runtime-configuration env-var docs.

## Risks / notes

- **Build weight & lock churn:** candle + transformers + tokenizers + hf-hub is a large dependency
  set. Kept entirely behind the default-off feature so the common build path is unaffected; CI
  should add one feature-on compile/test job to keep it from bit-rotting.
- **Model licensing / distribution:** we ship *no* model weights — the user supplies a path or an
  hf-hub repo id (Gemma's license is the user's responsibility). The default repo id is a
  convenience only.
- **`Send + Sync` over model state:** candle model state is not trivially `Sync`; the provider holds
  it behind the appropriate interior-mutability guard so `complete()` (which takes `&self`) can run
  a generation step, consistent with the `Provider: Send + Sync` seam.
- **CPU latency:** even a 1B quantized model is slow on CPU relative to a hosted API; this provider
  targets local/air-gapped/offline use, not throughput.
