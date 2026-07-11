# In-process `candle` Provider Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a feature-gated `CandleProvider` that runs a quantized Gemma 3 (GGUF) model in-process via candle, selectable as the engine's local router slot with `OTTO_CANDLE=1`.

**Architecture:** A new `crates/providers/src/candle.rs` module, compiled only under a default-off `candle` cargo feature, implements `otto_engine_core::Provider`. Pure helpers (model-source resolution, generation-config parsing, Gemma prompt templating, logits sampling) are free functions unit-tested without a model. `CandleProvider::complete()` loads the GGUF weights per call inside `tokio::task::spawn_blocking` (correct KV-cache isolation, trivially `Send + Sync`), templates the prompt, runs candle's forward/sample loop, and decodes. The engine's `build_local_provider` gains a candle branch (candle wins over ollama) behind the same feature.

**Tech Stack:** Rust 2024, candle-core / candle-transformers (`quantized_gemma3`), tokenizers, hf-hub, tokio, async-trait, anyhow.

## Global Constraints

- **Edition 2024, rust-version 1.85** (workspace-pinned; do not change).
- **Default features stay empty.** With the `candle` feature OFF (the default), `cargo build --workspace` and `cargo test --workspace` must be byte-for-byte unchanged — none of the candle deps compile in, `CandleProvider` does not exist, and the `OTTO_CANDLE` env var is ignored.
- **Determinism invariant.** Anything reading `OTTO_*` env vars lives in the engine wiring layer (`build_router`/`build_local_provider`) or behind the `candle` feature — never in `engine-core`. The offline-deterministic default path (both router slots `LocalProvider`) is untouched.
- **No `Provider`/`Router`/`CompleteRequest`/`CompleteResponse` trait or type changes.** The provider drops into the existing seam.
- **Provider is `Send + Sync`** and `async` (`async_trait`). `complete()` takes `&self`.
- **Commits:** conventional-commit style, one per task. Do NOT add any Claude self-attribution — no `Co-Authored-By: Claude` trailer, no "Generated with Claude Code" footer, no 🤖 marker.
- **Third-party API reconciliation:** the exact `candle-transformers` type path `quantized_gemma3::ModelWeights` and the crate versions below are best-effort; if a symbol differs at first compile, reconcile against candle's own `candle-examples/examples/quantized-gemma/main.rs` (the authoritative, compiling reference). The compile under `--features candle` is the check.

---

## File Structure

- `crates/providers/Cargo.toml` — optional candle deps; `candle` / `candle-cuda` / `candle-metal` features.
- `crates/providers/src/candle.rs` — **new.** All provider code + pure helpers + unit tests. One file, one responsibility (the candle provider).
- `crates/providers/src/lib.rs` — feature-gated `pub mod candle;` and `pub use candle::CandleProvider;`.
- `crates/providers/tests/fixtures/tokenizer.json` — **new.** Tiny WordLevel tokenizer fixture for the round-trip test.
- `crates/engine/Cargo.toml` — passthrough `candle` / `candle-cuda` / `candle-metal` features.
- `crates/engine/src/lib.rs` — `choose_local_slot` pure fn + candle branch in `build_local_provider`; `build_candle_provider` + `select_device` under the feature.
- `CLAUDE.md` — provider table row + runtime-config env-var docs.

---

## Task 1: Cargo feature scaffolding + model-source resolution

**Files:**
- Modify: `crates/providers/Cargo.toml`
- Create: `crates/providers/src/candle.rs`
- Modify: `crates/providers/src/lib.rs`
- Test: `crates/providers/src/candle.rs` (inline `#[cfg(test)]`)

**Interfaces:**
- Produces:
  - `pub enum ModelSource { LocalGguf(std::path::PathBuf), HubRepo(String) }`
  - `pub fn resolve_model_source(value: Option<String>) -> ModelSource`
  - `pub const DEFAULT_CANDLE_REPO: &str`

- [ ] **Step 1: Add optional deps + features to `crates/providers/Cargo.toml`**

Add under `[dependencies]`:

```toml
candle-core = { version = "0.9", optional = true }
candle-transformers = { version = "0.9", optional = true }
tokenizers = { version = "0.21", optional = true, default-features = false }
hf-hub = { version = "0.3", optional = true }
```

Add a new `[features]` section (the crate has none today):

```toml
[features]
candle = ["dep:candle-core", "dep:candle-transformers", "dep:tokenizers", "dep:hf-hub"]
candle-cuda = ["candle", "candle-core/cuda", "candle-transformers/cuda"]
candle-metal = ["candle", "candle-core/metal", "candle-transformers/metal"]
```

Add to `[dev-dependencies]` (used by later tasks' tests):

```toml
tempfile = { workspace = true }
```

- [ ] **Step 2: Create `crates/providers/src/candle.rs` with the module doc, imports, and model-source resolution**

```rust
//! `CandleProvider`: runs a quantized Gemma 3 (GGUF) model in-process via candle.
//! Feature-gated behind `candle`; fills the engine's local router slot (`OTTO_CANDLE=1`).
//! In local-file mode it performs no network I/O at all.

use std::path::{Path, PathBuf};

/// Default HuggingFace repo used when `OTTO_CANDLE_MODEL` is unset (a small Gemma 3
/// instruct QAT GGUF). The user is responsible for the model's license.
pub const DEFAULT_CANDLE_REPO: &str = "google/gemma-3-1b-it-qat-q4_0-gguf";

/// Where the model weights come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelSource {
    /// An existing `.gguf` file on disk (zero network). `tokenizer.json` is expected
    /// as a sibling of this file.
    LocalGguf(PathBuf),
    /// A HuggingFace repo id, resolved via `hf-hub` at construction time.
    HubRepo(String),
}

/// Resolve `OTTO_CANDLE_MODEL`'s value into a `ModelSource`: an existing `.gguf` path
/// loads locally; anything else is treated as a repo id; `None` uses the default repo.
pub fn resolve_model_source(value: Option<String>) -> ModelSource {
    match value {
        Some(v) => {
            let p = Path::new(&v);
            if p.extension().and_then(|e| e.to_str()) == Some("gguf") && p.is_file() {
                ModelSource::LocalGguf(p.to_path_buf())
            } else {
                ModelSource::HubRepo(v)
            }
        }
        None => ModelSource::HubRepo(DEFAULT_CANDLE_REPO.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn existing_gguf_path_resolves_to_local() {
        let dir = tempfile::tempdir().unwrap();
        let gguf = dir.path().join("model.gguf");
        std::fs::write(&gguf, b"not a real model").unwrap();
        let src = resolve_model_source(Some(gguf.to_string_lossy().into_owned()));
        assert_eq!(src, ModelSource::LocalGguf(gguf));
    }

    #[test]
    fn repo_id_resolves_to_hub() {
        let src = resolve_model_source(Some("google/gemma-3-1b-it-qat-q4_0-gguf".to_string()));
        assert_eq!(src, ModelSource::HubRepo("google/gemma-3-1b-it-qat-q4_0-gguf".to_string()));
    }

    #[test]
    fn nonexistent_gguf_path_falls_through_to_hub() {
        let src = resolve_model_source(Some("/no/such/model.gguf".to_string()));
        assert_eq!(src, ModelSource::HubRepo("/no/such/model.gguf".to_string()));
    }

    #[test]
    fn none_resolves_to_default_repo() {
        assert_eq!(
            resolve_model_source(None),
            ModelSource::HubRepo(DEFAULT_CANDLE_REPO.to_string())
        );
    }
}
```

- [ ] **Step 3: Wire the feature-gated module in `crates/providers/src/lib.rs`**

Add after the existing `pub mod scripted;` line:

```rust
#[cfg(feature = "candle")]
pub mod candle;
```

(Do NOT add `pub use candle::CandleProvider;` yet — the type is created in Task 5.)

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p otto-providers --features candle candle::tests`
Expected: PASS (4 tests).

- [ ] **Step 5: Verify the default build is untouched**

Run: `cargo test -p otto-providers`
Expected: PASS, and the candle module is not compiled (no candle deps built).

- [ ] **Step 6: Commit**

```bash
git add crates/providers/Cargo.toml crates/providers/src/candle.rs crates/providers/src/lib.rs
git commit -m "feat(providers): candle feature scaffolding + model-source resolution"
```

---

## Task 2: Generation config parsing

**Files:**
- Modify: `crates/providers/src/candle.rs`
- Test: `crates/providers/src/candle.rs` (inline)

**Interfaces:**
- Produces:
  - `pub struct GenConfig { pub max_tokens: usize, pub temperature: Option<f64>, pub top_p: Option<f64>, pub raw: bool, pub seed: u64 }` with `impl Default`
  - `pub fn parse_gen_config(get: impl Fn(&str) -> Option<String>) -> GenConfig`
  - `impl GenConfig { pub fn from_env() -> Self }`

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `candle.rs`:

```rust
#[test]
fn gen_config_defaults_when_env_absent() {
    let cfg = parse_gen_config(|_| None);
    assert_eq!(cfg.max_tokens, 512);
    assert_eq!(cfg.temperature, None);
    assert_eq!(cfg.top_p, None);
    assert!(!cfg.raw);
    assert_eq!(cfg.seed, 299792458);
}

#[test]
fn gen_config_parses_all_fields() {
    let get = |k: &str| match k {
        "OTTO_CANDLE_MAX_TOKENS" => Some("128".to_string()),
        "OTTO_CANDLE_TEMPERATURE" => Some("0.7".to_string()),
        "OTTO_CANDLE_TOP_P" => Some("0.9".to_string()),
        "OTTO_CANDLE_RAW" => Some("1".to_string()),
        "OTTO_CANDLE_SEED" => Some("42".to_string()),
        _ => None,
    };
    let cfg = parse_gen_config(get);
    assert_eq!(cfg.max_tokens, 128);
    assert_eq!(cfg.temperature, Some(0.7));
    assert_eq!(cfg.top_p, Some(0.9));
    assert!(cfg.raw);
    assert_eq!(cfg.seed, 42);
}

#[test]
fn gen_config_ignores_unparseable_and_nonpositive_temp() {
    let get = |k: &str| match k {
        "OTTO_CANDLE_MAX_TOKENS" => Some("banana".to_string()),
        "OTTO_CANDLE_TEMPERATURE" => Some("0".to_string()), // <= 0 => greedy (None)
        _ => None,
    };
    let cfg = parse_gen_config(get);
    assert_eq!(cfg.max_tokens, 512); // fell back to default
    assert_eq!(cfg.temperature, None);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p otto-providers --features candle candle::tests::gen_config`
Expected: FAIL (compile error — `GenConfig` / `parse_gen_config` not defined).

- [ ] **Step 3: Implement `GenConfig` + parsing**

Add near the top of `candle.rs` (after `resolve_model_source`):

```rust
const DEFAULT_MAX_TOKENS: usize = 512;
const DEFAULT_SEED: u64 = 299792458;

/// Generation parameters, sourced from `OTTO_CANDLE_*` env vars in production.
#[derive(Debug, Clone, PartialEq)]
pub struct GenConfig {
    pub max_tokens: usize,
    /// `None` => greedy (argmax). `Some(t > 0)` => temperature sampling.
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    /// Skip the Gemma instruct chat template (for base models).
    pub raw: bool,
    pub seed: u64,
}

impl Default for GenConfig {
    fn default() -> Self {
        Self {
            max_tokens: DEFAULT_MAX_TOKENS,
            temperature: None,
            top_p: None,
            raw: false,
            seed: DEFAULT_SEED,
        }
    }
}

/// Build a `GenConfig` from a key->value lookup (injectable for tests).
pub fn parse_gen_config(get: impl Fn(&str) -> Option<String>) -> GenConfig {
    let max_tokens = get("OTTO_CANDLE_MAX_TOKENS")
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_MAX_TOKENS);
    let temperature = get("OTTO_CANDLE_TEMPERATURE")
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|t| *t > 0.0);
    let top_p = get("OTTO_CANDLE_TOP_P").and_then(|s| s.parse::<f64>().ok());
    let raw = get("OTTO_CANDLE_RAW").as_deref() == Some("1");
    let seed = get("OTTO_CANDLE_SEED")
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_SEED);
    GenConfig { max_tokens, temperature, top_p, raw, seed }
}

impl GenConfig {
    /// Read the generation config from the process environment.
    pub fn from_env() -> Self {
        parse_gen_config(|k| std::env::var(k).ok())
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p otto-providers --features candle candle::tests::gen_config`
Expected: PASS (3 tests).

- [ ] **Step 5: Commit**

```bash
git add crates/providers/src/candle.rs
git commit -m "feat(providers): candle generation-config parsing"
```

---

## Task 3: Gemma prompt templating

**Files:**
- Modify: `crates/providers/src/candle.rs`
- Test: `crates/providers/src/candle.rs` (inline)

**Interfaces:**
- Produces: `pub fn gemma_prompt(user: &str, raw: bool) -> String`

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module:

```rust
#[test]
fn gemma_prompt_wraps_in_instruct_turns() {
    let p = gemma_prompt("hello", false);
    assert_eq!(
        p,
        "<start_of_turn>user\nhello<end_of_turn>\n<start_of_turn>model\n"
    );
}

#[test]
fn gemma_prompt_raw_passes_through() {
    assert_eq!(gemma_prompt("hello", true), "hello");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p otto-providers --features candle candle::tests::gemma_prompt`
Expected: FAIL (`gemma_prompt` not defined).

- [ ] **Step 3: Implement `gemma_prompt`**

Add to `candle.rs`:

```rust
/// Wrap a user prompt in Gemma's instruct chat template, unless `raw` is set.
pub fn gemma_prompt(user: &str, raw: bool) -> String {
    if raw {
        user.to_string()
    } else {
        format!("<start_of_turn>user\n{user}<end_of_turn>\n<start_of_turn>model\n")
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p otto-providers --features candle candle::tests::gemma_prompt`
Expected: PASS (2 tests).

- [ ] **Step 5: Commit**

```bash
git add crates/providers/src/candle.rs
git commit -m "feat(providers): candle Gemma instruct prompt template"
```

---

## Task 4: Logits sampling helper

**Files:**
- Modify: `crates/providers/src/candle.rs`
- Test: `crates/providers/src/candle.rs` (inline)

**Interfaces:**
- Consumes: `GenConfig` (Task 2)
- Produces: `pub(crate) fn build_logits_processor(gen: &GenConfig) -> candle_transformers::generation::LogitsProcessor`

- [ ] **Step 1: Write the failing test**

Add to the `tests` module:

```rust
#[test]
fn argmax_sampling_picks_highest_logit() {
    use candle_core::{Device, Tensor};
    let cfg = GenConfig { temperature: None, ..GenConfig::default() };
    let mut lp = build_logits_processor(&cfg);
    let logits = Tensor::new(&[0.1f32, 5.0, 0.2, 0.3], &Device::Cpu).unwrap();
    let token = lp.sample(&logits).unwrap();
    assert_eq!(token, 1);
}

#[test]
fn temperature_sampling_is_deterministic_for_a_fixed_seed() {
    use candle_core::{Device, Tensor};
    let cfg = GenConfig { temperature: Some(0.8), top_p: Some(0.95), seed: 7, ..GenConfig::default() };
    let logits = Tensor::new(&[0.1f32, 5.0, 0.2, 0.3], &Device::Cpu).unwrap();
    let a = { let mut lp = build_logits_processor(&cfg); lp.sample(&logits).unwrap() };
    let b = { let mut lp = build_logits_processor(&cfg); lp.sample(&logits).unwrap() };
    assert_eq!(a, b);
    assert!((a as usize) < 4);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p otto-providers --features candle candle::tests::_sampling`
Expected: FAIL (`build_logits_processor` not defined).

- [ ] **Step 3: Implement `build_logits_processor`**

Add to `candle.rs`:

```rust
/// Build candle's sampler from our config: greedy (argmax) by default, or
/// temperature / temperature+top-p when configured.
pub(crate) fn build_logits_processor(
    gen: &GenConfig,
) -> candle_transformers::generation::LogitsProcessor {
    use candle_transformers::generation::{LogitsProcessor, Sampling};
    let sampling = match gen.temperature {
        None => Sampling::ArgMax,
        Some(t) => match gen.top_p {
            Some(p) => Sampling::TopP { p, temperature: t },
            None => Sampling::All { temperature: t },
        },
    };
    LogitsProcessor::from_sampling(gen.seed, sampling)
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p otto-providers --features candle candle::tests::_sampling`
Expected: PASS (2 tests).

- [ ] **Step 5: Commit**

```bash
git add crates/providers/src/candle.rs
git commit -m "feat(providers): candle logits sampler (greedy default, temp/top-p opt-in)"
```

---

## Task 5: `CandleProvider` — construction, generation, and the `Provider` impl

**Files:**
- Modify: `crates/providers/src/candle.rs`
- Modify: `crates/providers/src/lib.rs`
- Create: `crates/providers/tests/fixtures/tokenizer.json`
- Test: `crates/providers/src/candle.rs` (inline)

**Interfaces:**
- Consumes: `ModelSource` (T1), `GenConfig` (T2), `gemma_prompt` (T3), `build_logits_processor` (T4), `otto_engine_core::traits::Provider`, `otto_engine_core::types::{CompleteRequest, CompleteResponse, Usage}`.
- Produces:
  - `pub struct CandleProvider` with `pub fn new(source: ModelSource, gen: GenConfig, device: candle_core::Device) -> anyhow::Result<Self>`
  - `pub fn select_device() -> candle_core::Device`
  - `impl Provider for CandleProvider` (`id() -> "candle"`, `complete`)
  - re-export `pub use candle::CandleProvider;`

- [ ] **Step 1: Create the tokenizer fixture `crates/providers/tests/fixtures/tokenizer.json`**

```json
{
  "version": "1.0",
  "truncation": null,
  "padding": null,
  "added_tokens": [
    { "id": 3, "content": "<end_of_turn>", "single_word": false, "lstrip": false, "rstrip": false, "normalized": false, "special": true }
  ],
  "normalizer": null,
  "pre_tokenizer": { "type": "Whitespace" },
  "post_processor": null,
  "decoder": null,
  "model": {
    "type": "WordLevel",
    "vocab": { "hello": 0, "world": 1, "[UNK]": 2, "<end_of_turn>": 3 },
    "unk_token": "[UNK]"
  }
}
```

- [ ] **Step 2: Write the failing tests**

Add to the `tests` module in `candle.rs`:

```rust
const FIXTURE_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

fn fixture_tokenizer_dir() -> tempfile::TempDir {
    // Copy the fixture tokenizer.json next to a stub .gguf so `new` can locate both.
    let dir = tempfile::tempdir().unwrap();
    std::fs::copy(
        format!("{FIXTURE_DIR}/tokenizer.json"),
        dir.path().join("tokenizer.json"),
    )
    .unwrap();
    dir
}

#[test]
fn provider_id_is_candle() {
    let dir = fixture_tokenizer_dir();
    let gguf = dir.path().join("model.gguf");
    std::fs::write(&gguf, b"stub").unwrap();
    let p = CandleProvider::new(
        ModelSource::LocalGguf(gguf),
        GenConfig::default(),
        candle_core::Device::Cpu,
    )
    .unwrap();
    assert_eq!(p.id(), "candle");
}

#[test]
fn new_errors_when_gguf_missing() {
    let dir = fixture_tokenizer_dir();
    let err = CandleProvider::new(
        ModelSource::LocalGguf(dir.path().join("absent.gguf")),
        GenConfig::default(),
        candle_core::Device::Cpu,
    )
    .unwrap_err();
    assert!(err.to_string().contains("gguf"));
}

#[test]
fn new_errors_when_tokenizer_missing() {
    let dir = tempfile::tempdir().unwrap();
    let gguf = dir.path().join("model.gguf");
    std::fs::write(&gguf, b"stub").unwrap();
    let err = CandleProvider::new(
        ModelSource::LocalGguf(gguf),
        GenConfig::default(),
        candle_core::Device::Cpu,
    )
    .unwrap_err();
    assert!(err.to_string().contains("tokenizer"));
}

// End-to-end generation: needs a real Gemma GGUF supplied via OTTO_CANDLE_MODEL.
// Ignored by default; run manually with:
//   OTTO_CANDLE_MODEL=/path/model.gguf cargo test -p otto-providers --features candle \
//     candle::tests::end_to_end_generation -- --ignored --nocapture
#[ignore]
#[tokio::test(flavor = "multi_thread")]
async fn end_to_end_generation() {
    use otto_engine_core::traits::Provider;
    use otto_engine_core::types::CompleteRequest;
    let src = resolve_model_source(std::env::var("OTTO_CANDLE_MODEL").ok());
    let cfg = GenConfig { max_tokens: 32, ..GenConfig::default() };
    let p = CandleProvider::new(src, cfg, select_device()).unwrap();
    let out = p
        .complete(CompleteRequest { prompt: "Say hello in one word.".into() })
        .await
        .unwrap();
    assert!(!out.text.trim().is_empty());
    assert!(out.usage.unwrap().output_tokens > 0);
}
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test -p otto-providers --features candle candle::tests::provider_id`
Expected: FAIL (`CandleProvider` not defined).

- [ ] **Step 4: Implement `CandleProvider`, `new`, `locate_files`, hub download, `generate`, `select_device`, and the `Provider` impl**

Add the imports at the top of `candle.rs` (below the existing `use std::path...`):

```rust
use std::sync::Arc;

use async_trait::async_trait;
use candle_core::{Device, Tensor};
use otto_engine_core::traits::Provider;
use otto_engine_core::types::{CompleteRequest, CompleteResponse, Usage};
use tokenizers::Tokenizer;
```

Add the provider and helpers to `candle.rs`:

```rust
/// In-process quantized Gemma 3 provider. Weights are (re)loaded per `complete()` call
/// inside a blocking task, giving each completion an isolated KV cache and keeping the
/// provider trivially `Send + Sync`. Suited to local / air-gapped use, not throughput.
pub struct CandleProvider {
    gguf: PathBuf,
    tokenizer: Arc<Tokenizer>,
    device: Device,
    gen: GenConfig,
}

impl CandleProvider {
    /// Resolve the model + tokenizer files and load the tokenizer once. Model weights
    /// themselves are loaded lazily on each `complete()` call.
    pub fn new(source: ModelSource, gen: GenConfig, device: Device) -> anyhow::Result<Self> {
        let (gguf, tokenizer_json) = locate_files(source)?;
        let tokenizer = Tokenizer::from_file(&tokenizer_json).map_err(anyhow::Error::msg)?;
        Ok(Self { gguf, tokenizer: Arc::new(tokenizer), device, gen })
    }
}

/// Pick the compute device: an accelerator when built with `candle-cuda`/`candle-metal`
/// and available, else CPU.
pub fn select_device() -> Device {
    #[cfg(feature = "candle-cuda")]
    {
        if let Ok(d) = Device::cuda_if_available(0) {
            return d;
        }
    }
    #[cfg(feature = "candle-metal")]
    {
        if let Ok(d) = Device::new_metal(0) {
            return d;
        }
    }
    Device::Cpu
}

/// Resolve a `ModelSource` to concrete (gguf, tokenizer.json) paths.
fn locate_files(source: ModelSource) -> anyhow::Result<(PathBuf, PathBuf)> {
    match source {
        ModelSource::LocalGguf(gguf) => {
            if !gguf.is_file() {
                anyhow::bail!("candle model gguf not found: {}", gguf.display());
            }
            let dir = gguf.parent().unwrap_or_else(|| Path::new("."));
            let tok = dir.join("tokenizer.json");
            if !tok.is_file() {
                anyhow::bail!(
                    "candle tokenizer.json not found next to model: {}",
                    tok.display()
                );
            }
            Ok((gguf, tok))
        }
        ModelSource::HubRepo(repo) => download_from_hub(&repo),
    }
}

/// Download the GGUF + tokenizer from a HuggingFace repo into the hf-hub cache.
/// The GGUF filename within the repo is taken from `OTTO_CANDLE_GGUF_FILE`
/// (default `model.gguf`). Network at load time only — never during inference.
fn download_from_hub(repo: &str) -> anyhow::Result<(PathBuf, PathBuf)> {
    use hf_hub::api::sync::Api;
    let file = std::env::var("OTTO_CANDLE_GGUF_FILE").unwrap_or_else(|_| "model.gguf".to_string());
    let api = Api::new()?;
    let model = api.model(repo.to_string());
    let gguf = model.get(&file)?;
    let tok = model.get("tokenizer.json")?;
    Ok((gguf, tok))
}

/// Load the GGUF weights and run the generation loop. Blocking; call under
/// `spawn_blocking`. Returns the decoded text and token usage.
fn generate(
    gguf: &Path,
    device: &Device,
    tokenizer: &Tokenizer,
    gen: &GenConfig,
    prompt: &str,
) -> anyhow::Result<(String, Usage)> {
    use candle_core::quantized::gguf_file;
    use candle_transformers::models::quantized_gemma3::ModelWeights;

    let mut file = std::fs::File::open(gguf)?;
    let content = gguf_file::Content::read(&mut file).map_err(|e| e.with_path(gguf))?;
    let mut model = ModelWeights::from_gguf(content, &mut file, device)?;

    let encoding = tokenizer.encode(prompt, true).map_err(anyhow::Error::msg)?;
    let prompt_tokens: Vec<u32> = encoding.get_ids().to_vec();
    let input_tokens = prompt_tokens.len() as u32;

    let eos_id = tokenizer.get_vocab(true).get("<end_of_turn>").copied();
    let mut logits_processor = build_logits_processor(gen);

    // Prompt pass (whole prompt at position 0), then autoregressive decode.
    let input = Tensor::new(prompt_tokens.as_slice(), device)?.unsqueeze(0)?;
    let logits = model.forward(&input, 0)?.squeeze(0)?;
    let mut next = logits_processor.sample(&logits)?;

    let mut out_ids: Vec<u32> = Vec::new();
    let mut pos = prompt_tokens.len();
    for _ in 0..gen.max_tokens {
        if Some(next) == eos_id {
            break;
        }
        out_ids.push(next);
        let input = Tensor::new(&[next], device)?.unsqueeze(0)?;
        let logits = model.forward(&input, pos)?.squeeze(0)?;
        next = logits_processor.sample(&logits)?;
        pos += 1;
    }

    let output_tokens = out_ids.len() as u32;
    let text = tokenizer.decode(&out_ids, true).map_err(anyhow::Error::msg)?;
    Ok((text, Usage { input_tokens, output_tokens }))
}

#[async_trait]
impl Provider for CandleProvider {
    fn id(&self) -> &str {
        "candle"
    }

    async fn complete(&self, req: CompleteRequest) -> anyhow::Result<CompleteResponse> {
        let gguf = self.gguf.clone();
        let device = self.device.clone();
        let tokenizer = self.tokenizer.clone();
        let gen = self.gen.clone();
        let prompt = gemma_prompt(&req.prompt, gen.raw);

        let (text, usage) = tokio::task::spawn_blocking(move || {
            generate(&gguf, &device, &tokenizer, &gen, &prompt)
        })
        .await??;

        Ok(CompleteResponse { text, usage: Some(usage) })
    }
}
```

Add `tokio` to `crates/providers/Cargo.toml` `[dependencies]` (needed for `spawn_blocking`; it is only compiled under the `candle` feature via the optional dep, so gate it):

```toml
tokio = { workspace = true, optional = true, features = ["rt"] }
```

and extend the `candle` feature to include it:

```toml
candle = ["dep:candle-core", "dep:candle-transformers", "dep:tokenizers", "dep:hf-hub", "dep:tokio"]
```

- [ ] **Step 5: Add the re-export to `crates/providers/src/lib.rs`**

```rust
#[cfg(feature = "candle")]
pub use candle::CandleProvider;
```

- [ ] **Step 6: Run the construction tests to verify they pass**

Run: `cargo test -p otto-providers --features candle candle::tests`
Expected: PASS for `provider_id_is_candle`, `new_errors_when_gguf_missing`, `new_errors_when_tokenizer_missing` (and all Task 1–4 tests). `end_to_end_generation` is reported as ignored.

- [ ] **Step 7: Verify the default build is still untouched**

Run: `cargo build --workspace && cargo test -p otto-providers`
Expected: PASS; no candle deps compiled in the default build.

- [ ] **Step 8: Commit**

```bash
git add crates/providers/Cargo.toml crates/providers/src/candle.rs crates/providers/src/lib.rs crates/providers/tests/fixtures/tokenizer.json
git commit -m "feat(providers): CandleProvider (quantized Gemma GGUF, per-call load)"
```

---

## Task 6: Engine wiring + docs

**Files:**
- Modify: `crates/engine/Cargo.toml`
- Modify: `crates/engine/src/lib.rs`
- Modify: `CLAUDE.md`
- Test: `crates/engine/src/lib.rs` (inline)

**Interfaces:**
- Consumes: `otto_providers::candle::{resolve_model_source, GenConfig, select_device, CandleProvider}` (Task 5), the existing `build_local_provider` (`crates/engine/src/lib.rs:70`).
- Produces: `enum LocalSlot { Candle, Ollama, Local }`, `fn choose_local_slot(candle_on: bool, ollama_on: bool) -> LocalSlot`.

- [ ] **Step 1: Add passthrough features to `crates/engine/Cargo.toml`**

Extend the `[features]` section (currently just `firecracker`):

```toml
[features]
firecracker = ["otto-remote/firecracker"]
candle = ["otto-providers/candle"]
candle-cuda = ["candle", "otto-providers/candle-cuda"]
candle-metal = ["candle", "otto-providers/candle-metal"]
```

- [ ] **Step 2: Write the failing test for `choose_local_slot`**

Add to the existing `#[cfg(test)] mod tests` in `crates/engine/src/lib.rs`:

```rust
#[test]
fn local_slot_precedence_candle_wins_over_ollama() {
    assert_eq!(choose_local_slot(true, true), LocalSlot::Candle);
    assert_eq!(choose_local_slot(true, false), LocalSlot::Candle);
    assert_eq!(choose_local_slot(false, true), LocalSlot::Ollama);
    assert_eq!(choose_local_slot(false, false), LocalSlot::Local);
}
```

- [ ] **Step 3: Run the test to verify it fails**

Run: `cargo test -p otto-engine local_slot_precedence`
Expected: FAIL (`choose_local_slot` / `LocalSlot` not defined).

- [ ] **Step 4: Implement the selection logic and rewire `build_local_provider`**

In `crates/engine/src/lib.rs`, replace the existing `build_local_provider` (lines ~69–78) with:

```rust
/// Which provider fills the local router slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LocalSlot {
    Candle,
    Ollama,
    Local,
}

/// Precedence for the local slot: candle (in-process) > ollama (HTTP) > offline Local.
fn choose_local_slot(candle_on: bool, ollama_on: bool) -> LocalSlot {
    if candle_on {
        LocalSlot::Candle
    } else if ollama_on {
        LocalSlot::Ollama
    } else {
        LocalSlot::Local
    }
}

/// Construct the local provider slot from the environment (shared by both router builders).
fn build_local_provider() -> Arc<dyn Provider> {
    // `OTTO_CANDLE` is honored only when the `candle` feature is compiled in.
    let candle_on =
        cfg!(feature = "candle") && std::env::var("OTTO_CANDLE").as_deref() == Ok("1");
    let ollama_on = std::env::var("OTTO_OLLAMA").as_deref() == Ok("1");
    if candle_on && ollama_on {
        eprintln!(
            "warning: both OTTO_CANDLE and OTTO_OLLAMA are set; using the in-process candle provider"
        );
    }
    match choose_local_slot(candle_on, ollama_on) {
        LocalSlot::Candle => {
            #[cfg(feature = "candle")]
            {
                build_candle_provider()
            }
            #[cfg(not(feature = "candle"))]
            {
                unreachable!("candle_on is false without the candle feature")
            }
        }
        LocalSlot::Ollama => {
            let model = std::env::var("OTTO_OLLAMA_MODEL")
                .unwrap_or_else(|_| DEFAULT_OLLAMA_MODEL.to_string());
            Arc::new(OllamaProvider::local_default(model))
        }
        LocalSlot::Local => Arc::new(LocalProvider::new()),
    }
}

/// Build the candle provider from `OTTO_CANDLE_*` env vars, falling back to the offline
/// `LocalProvider` (with a warning) if the model can't be loaded.
#[cfg(feature = "candle")]
fn build_candle_provider() -> Arc<dyn Provider> {
    use otto_providers::candle::{CandleProvider, GenConfig, resolve_model_source, select_device};
    let source = resolve_model_source(std::env::var("OTTO_CANDLE_MODEL").ok());
    match CandleProvider::new(source, GenConfig::from_env(), select_device()) {
        Ok(p) => Arc::new(p),
        Err(e) => {
            eprintln!("warning: candle provider unavailable ({e}); using offline LocalProvider");
            Arc::new(LocalProvider::new())
        }
    }
}
```

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo test -p otto-engine local_slot_precedence`
Expected: PASS.

- [ ] **Step 6: Verify default + feature-on builds**

Run: `cargo build --workspace` (default — must be unchanged) then `cargo build -p otto-engine --features candle`.
Expected: both PASS.

- [ ] **Step 7: Update `CLAUDE.md`**

In the crate table, extend the `providers` row's provider list to include:
`, and \`CandleProvider\` (in-process quantized Gemma 3 GGUF via candle, behind the default-off \`candle\` feature)`.

In the **Runtime configuration (env vars)** section, add under the local-slot bullets:

```markdown
- `OTTO_CANDLE=1` — use the in-process `CandleProvider` for the local slot (requires building with `--features candle`; wins over `OTTO_OLLAMA` when both are set). Model from `OTTO_CANDLE_MODEL` (an existing `.gguf` path → loaded locally with no network; otherwise a HuggingFace repo id, default `google/gemma-3-1b-it-qat-q4_0-gguf`, with the in-repo filename from `OTTO_CANDLE_GGUF_FILE`, default `model.gguf`). Generation tuned by `OTTO_CANDLE_MAX_TOKENS` (default 512), `OTTO_CANDLE_TEMPERATURE`/`OTTO_CANDLE_TOP_P` (greedy/argmax when unset), `OTTO_CANDLE_RAW=1` (skip the Gemma instruct template), `OTTO_CANDLE_SEED`. Optional GPU builds: `--features candle-cuda` / `--features candle-metal`.
```

- [ ] **Step 8: Commit**

```bash
git add crates/engine/Cargo.toml crates/engine/src/lib.rs CLAUDE.md
git commit -m "feat(engine): wire in-process candle provider into the local router slot"
```

---

## Self-Review

**Spec coverage** (checked against `docs/superpowers/specs/2026-07-10-providers-candle-design.md`):

- §1 Crate placement & feature gating → Task 1 (Cargo features/deps, feature-gated module) + Task 5 (`pub use`) + Task 6 (engine passthrough features). ✅
- §2 Model loading & config (source resolution, tokenizer, device, `id()`) → Task 1 (`resolve_model_source`), Task 5 (`locate_files`, tokenizer load, `select_device`, `download_from_hub`, `id()`). ✅
- §3 Generation (Gemma template, greedy-default sampling, max-tokens/EOS, usage) → Task 3 (template), Task 4 (sampler), Task 5 (`generate` loop, EOS stop, `Usage`). ✅
- §4 Router wiring (`OTTO_CANDLE` selection, candle-wins precedence + warning, no trait changes) → Task 6. ✅
- §5 Testing (pure-logic unit tests feature-on; `#[ignore]` e2e; default suite untouched) → Tasks 1–4 unit tests, Task 5 construction tests + `#[ignore]` e2e, "default build untouched" verify steps in Tasks 1/5/6. ✅
- §6 Out of scope (embeddings, multi-arch, streaming, backend abstraction, no default-path change) → nothing in the plan violates these. ✅

**Placeholder scan:** No TBD/TODO/"handle edge cases"/"similar to Task N". Every code step shows the full code. The one third-party-API caveat (`quantized_gemma3::ModelWeights` path / crate versions) is called out in Global Constraints with a concrete reconciliation reference (candle's own example) and a compile-based check — not a placeholder.

**Type consistency:** `GenConfig` fields (`max_tokens: usize`, `temperature: Option<f64>`, `top_p: Option<f64>`, `raw: bool`, `seed: u64`) are consistent across Tasks 2/4/5/6. `ModelSource` variants (`LocalGguf`/`HubRepo`) consistent across Tasks 1/5. `build_logits_processor` (T4) consumed by `generate` (T5) with matching signature. `resolve_model_source`/`GenConfig::from_env`/`select_device`/`CandleProvider::new` signatures used in Task 6 match their Task 1/2/5 definitions. `LocalSlot`/`choose_local_slot` consistent within Task 6.
