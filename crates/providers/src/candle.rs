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
}
