//! otto provider implementations (in-process libraries behind `otto_engine_core::Provider`).

pub mod anthropic;
pub mod gemini;
pub mod local;
pub mod ollama;
pub mod openai;
pub mod scripted;

pub use anthropic::AnthropicProvider;
pub use gemini::GeminiProvider;
pub use local::LocalProvider;
pub use ollama::OllamaProvider;
pub use openai::OpenAiProvider;
pub use scripted::ScriptedProvider;
