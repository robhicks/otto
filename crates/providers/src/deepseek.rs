//! `DeepSeekProvider`: talks to the DeepSeek Chat Completions API over HTTP.
//! The wire format is OpenAI-compatible, so this shares `OpenAiCompatibleProvider` with
//! `OpenAiProvider`. Remote, requires an API key. `base_url` is configurable for testing.

use super::openai_compatible::{OpenAiCompatibleProvider, always_max_tokens};
use async_trait::async_trait;
use otto_engine_core::traits::Provider;
use otto_engine_core::types::{CompleteRequest, CompleteResponse};

pub struct DeepSeekProvider {
    inner: OpenAiCompatibleProvider,
}

impl DeepSeekProvider {
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            inner: OpenAiCompatibleProvider::new(
                base_url.into(),
                "/chat/completions",
                api_key.into(),
                model.into(),
                always_max_tokens,
            ),
        }
    }

    /// The production API base URL. DeepSeek's OpenAI-compatible endpoint lives at
    /// `/chat/completions` on this base (the `/v1` prefix is also accepted, not versioned).
    pub fn api_base_default() -> &'static str {
        "https://api.deepseek.com"
    }
}

#[async_trait]
impl Provider for DeepSeekProvider {
    fn id(&self) -> &str {
        "deepseek"
    }

    async fn complete(&self, req: CompleteRequest) -> anyhow::Result<CompleteResponse> {
        self.inner.complete(req).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_partial_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn deepseek_posts_chat_with_bearer_and_parses_text() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(header("authorization", "Bearer test-key"))
            .and(body_partial_json(serde_json::json!({
                "model": "deepseek-v4-flash"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{ "message": { "role": "assistant", "content": "hello from deepseek" } }]
            })))
            .mount(&server)
            .await;

        let provider = DeepSeekProvider::new(server.uri(), "test-key", "deepseek-v4-flash");
        let out = provider
            .complete(CompleteRequest {
                prompt: "hi".into(),
            })
            .await
            .unwrap();
        assert_eq!(out.text, "hello from deepseek");
        assert_eq!(provider.id(), "deepseek");
    }

    #[tokio::test]
    async fn deepseek_parses_usage_tokens() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{ "message": { "role": "assistant", "content": "hi" } }],
                "usage": { "prompt_tokens": 12, "completion_tokens": 34 }
            })))
            .mount(&server)
            .await;
        let provider = DeepSeekProvider::new(server.uri(), "k", "deepseek-v4-flash");
        let out = provider
            .complete(CompleteRequest {
                prompt: "hi".into(),
            })
            .await
            .unwrap();
        assert_eq!(
            out.usage,
            Some(otto_engine_core::types::Usage {
                input_tokens: 12,
                output_tokens: 34
            })
        );
    }

    #[tokio::test]
    async fn deepseek_surfaces_http_errors() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;

        let provider = DeepSeekProvider::new(server.uri(), "bad-key", "deepseek-v4-flash");
        let err = provider
            .complete(CompleteRequest {
                prompt: "hi".into(),
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("401") || err.to_string().contains("status"));
    }

    #[tokio::test]
    async fn deepseek_sends_max_tokens_in_body() {
        let server = MockServer::start().await;
        // The mock only matches if the request body contains max_tokens (the parameter DeepSeek
        // accepts on all models, including the reasoning tier).
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(body_partial_json(serde_json::json!({ "max_tokens": 4096 })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{ "message": { "role": "assistant", "content": "ok" } }]
            })))
            .mount(&server)
            .await;

        let provider = DeepSeekProvider::new(server.uri(), "k", "deepseek-reasoner");
        let out = provider
            .complete(CompleteRequest {
                prompt: "hi".into(),
            })
            .await
            .unwrap();
        assert_eq!(out.text, "ok");
    }

    #[tokio::test]
    async fn deepseek_tolerates_a_base_url_with_a_trailing_slash() {
        // Regression for #112, mirroring the OpenAI case against the other path suffix
        // (`/chat/completions`, no `/v1` segment).
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{ "message": { "role": "assistant", "content": "ok" } }]
            })))
            .mount(&server)
            .await;

        let provider =
            DeepSeekProvider::new(format!("{}/", server.uri()), "k", "deepseek-v4-flash");
        let out = provider
            .complete(CompleteRequest {
                prompt: "hi".into(),
            })
            .await
            .unwrap();
        assert_eq!(out.text, "ok");
    }

    #[tokio::test]
    async fn deepseek_returns_empty_text_for_empty_choices() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({ "choices": [] })),
            )
            .mount(&server)
            .await;

        let provider = DeepSeekProvider::new(server.uri(), "k", "deepseek-v4-flash");
        let out = provider
            .complete(CompleteRequest {
                prompt: "hi".into(),
            })
            .await
            .unwrap();
        assert_eq!(out.text, "");
        assert_eq!(out.usage, None);
    }
}
