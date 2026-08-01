//! otto provider implementations (in-process libraries behind `otto_engine_core::Provider`).

pub mod anthropic;
mod base_url;
#[cfg(feature = "candle")]
pub mod candle;
pub mod deepseek;
pub mod gemini;
pub mod local;
pub mod ollama;
pub mod openai;
mod openai_compatible;
pub mod scripted;

pub use anthropic::AnthropicProvider;
pub use base_url::{BaseUrlError, validate_base_url};
#[cfg(feature = "candle")]
pub use candle::CandleProvider;
pub use deepseek::DeepSeekProvider;
pub use gemini::GeminiProvider;
pub use local::LocalProvider;
pub use ollama::OllamaProvider;
pub use openai::OpenAiProvider;
pub use scripted::ScriptedProvider;
