//! `CandleProvider`: runs a quantized Gemma 3 (GGUF) model in-process via candle.
//! Feature-gated behind `candle`; fills the engine's local router slot (`OTTO_CANDLE=1`).
//! In local-file mode it performs no network I/O at all.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use candle_core::{Device, Tensor};
use otto_engine_core::traits::Provider;
use otto_engine_core::types::{CompleteRequest, CompleteResponse, Usage};
use tokenizers::Tokenizer;

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
    GenConfig {
        max_tokens,
        temperature,
        top_p,
        raw,
        seed,
    }
}

impl GenConfig {
    /// Read the generation config from the process environment.
    pub fn from_env() -> Self {
        parse_gen_config(|k| std::env::var(k).ok())
    }
}

/// Wrap a user prompt in Gemma's instruct chat template, unless `raw` is set.
pub fn gemma_prompt(user: &str, raw: bool) -> String {
    if raw {
        user.to_string()
    } else {
        format!("<start_of_turn>user\n{user}<end_of_turn>\n<start_of_turn>model\n")
    }
}

/// Build candle's sampler from our config: greedy (argmax) by default, or
/// temperature / temperature+top-p when configured.
pub(crate) fn build_logits_processor(
    cfg: &GenConfig,
) -> candle_transformers::generation::LogitsProcessor {
    use candle_transformers::generation::{LogitsProcessor, Sampling};
    let sampling = match cfg.temperature {
        None => Sampling::ArgMax,
        Some(t) => match cfg.top_p {
            Some(p) => Sampling::TopP { p, temperature: t },
            None => Sampling::All { temperature: t },
        },
    };
    LogitsProcessor::from_sampling(cfg.seed, sampling)
}

/// In-process quantized Gemma 3 provider. Weights are (re)loaded per `complete()` call
/// inside a blocking task, giving each completion an isolated KV cache and keeping the
/// provider trivially `Send + Sync`. Suited to local / air-gapped use, not throughput.
#[derive(Debug)]
pub struct CandleProvider {
    gguf: PathBuf,
    tokenizer: Arc<Tokenizer>,
    device: Device,
    gen_config: GenConfig,
}

impl CandleProvider {
    /// Resolve the model + tokenizer files and load the tokenizer once. Model weights
    /// themselves are loaded lazily on each `complete()` call.
    pub fn new(source: ModelSource, cfg: GenConfig, device: Device) -> anyhow::Result<Self> {
        let (gguf, tokenizer_json) = locate_files(source)?;
        let tokenizer = Tokenizer::from_file(&tokenizer_json).map_err(anyhow::Error::msg)?;
        Ok(Self {
            gguf,
            tokenizer: Arc::new(tokenizer),
            device,
            gen_config: cfg,
        })
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
    cfg: &GenConfig,
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
    let mut logits_processor = build_logits_processor(cfg);

    // Prompt pass (whole prompt at position 0), then autoregressive decode.
    let input = Tensor::new(prompt_tokens.as_slice(), device)?.unsqueeze(0)?;
    let logits = model.forward(&input, 0)?.squeeze(0)?;
    let mut next = logits_processor.sample(&logits)?;

    let mut out_ids: Vec<u32> = Vec::new();
    for step in 0..cfg.max_tokens {
        if Some(next) == eos_id {
            break;
        }
        out_ids.push(next);
        let pos = prompt_tokens.len() + step;
        let input = Tensor::new(&[next], device)?.unsqueeze(0)?;
        let logits = model.forward(&input, pos)?.squeeze(0)?;
        next = logits_processor.sample(&logits)?;
    }

    let output_tokens = out_ids.len() as u32;
    let text = tokenizer
        .decode(&out_ids, true)
        .map_err(anyhow::Error::msg)?;
    Ok((
        text,
        Usage {
            input_tokens,
            output_tokens,
        },
    ))
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
        let cfg = self.gen_config.clone();
        let prompt = gemma_prompt(&req.prompt, cfg.raw);

        let (text, usage) = tokio::task::spawn_blocking(move || {
            generate(&gguf, &device, &tokenizer, &cfg, &prompt)
        })
        .await??;

        Ok(CompleteResponse {
            text,
            usage: Some(usage),
        })
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
        assert_eq!(
            src,
            ModelSource::HubRepo("google/gemma-3-1b-it-qat-q4_0-gguf".to_string())
        );
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

    #[test]
    fn argmax_sampling_picks_highest_logit() {
        use candle_core::{Device, Tensor};
        let cfg = GenConfig {
            temperature: None,
            ..GenConfig::default()
        };
        let mut lp = build_logits_processor(&cfg);
        let logits = Tensor::new(&[0.1f32, 5.0, 0.2, 0.3], &Device::Cpu).unwrap();
        let token = lp.sample(&logits).unwrap();
        assert_eq!(token, 1);
    }

    #[test]
    fn temperature_sampling_is_deterministic_for_a_fixed_seed() {
        use candle_core::{Device, Tensor};
        let cfg = GenConfig {
            temperature: Some(0.8),
            top_p: Some(0.95),
            seed: 7,
            ..GenConfig::default()
        };
        let logits = Tensor::new(&[0.1f32, 5.0, 0.2, 0.3], &Device::Cpu).unwrap();
        let a = {
            let mut lp = build_logits_processor(&cfg);
            lp.sample(&logits).unwrap()
        };
        let b = {
            let mut lp = build_logits_processor(&cfg);
            lp.sample(&logits).unwrap()
        };
        assert_eq!(a, b);
        assert!((a as usize) < 4);
    }

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
        let cfg = GenConfig {
            max_tokens: 32,
            ..GenConfig::default()
        };
        let p = CandleProvider::new(src, cfg, select_device()).unwrap();
        let out = p
            .complete(CompleteRequest {
                prompt: "Say hello in one word.".into(),
            })
            .await
            .unwrap();
        assert!(!out.text.trim().is_empty());
        assert!(out.usage.unwrap().output_tokens > 0);
    }
}
