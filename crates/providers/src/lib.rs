//! otto provider implementations (in-process libraries behind `otto_engine_core::Provider`).

pub mod anthropic;
pub mod local;
pub mod ollama;

pub use anthropic::AnthropicProvider;
pub use local::LocalProvider;
pub use ollama::OllamaProvider;
