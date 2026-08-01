//! otto provider implementations (in-process libraries behind `otto_engine_core::Provider`).

pub mod anthropic;
#[cfg(feature = "candle")]
pub mod candle;
pub mod deepseek;
pub mod gemini;
pub mod local;
pub mod ollama;
pub mod openai;
pub mod scripted;

pub use anthropic::AnthropicProvider;
#[cfg(feature = "candle")]
pub use candle::CandleProvider;
pub use deepseek::DeepSeekProvider;
pub use gemini::GeminiProvider;
pub use local::LocalProvider;
pub use ollama::OllamaProvider;
pub use openai::OpenAiProvider;
pub use scripted::ScriptedProvider;
