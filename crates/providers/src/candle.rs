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
}
