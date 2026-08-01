//! `OpenAiProvider`: talks to the OpenAI Chat Completions API over HTTP.
//! Remote, requires an API key. `base_url` is configurable for testing.
//! The wire implementation is shared with `DeepSeekProvider` (see `openai_compatible`).

use super::openai_compatible::{OpenAiCompatibleProvider, always_max_tokens};
use async_trait::async_trait;
use otto_engine_core::traits::Provider;
use otto_engine_core::types::{CompleteRequest, CompleteResponse};

pub struct OpenAiProvider {
    inner: OpenAiCompatibleProvider,
}

impl OpenAiProvider {
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            inner: OpenAiCompatibleProvider::new(
                base_url.into(),
                "/v1/chat/completions",
                api_key.into(),
                model.into(),
                o_series_token_fields,
            ),
        }
    }

    /// The production API base URL.
    pub fn api_base_default() -> &'static str {
        "https://api.openai.com"
    }
}

/// OpenAI reasoning (o-series) models reject the `max_tokens` field and require
/// `max_completion_tokens` instead.
fn o_series_token_fields(model: &str, budget: u32) -> (Option<u32>, Option<u32>) {
    if is_o_series(model) {
        (None, Some(budget))
    } else {
        always_max_tokens(model, budget)
    }
}

fn is_o_series(model: &str) -> bool {
    model.starts_with("o1") || model.starts_with("o3") || model.starts_with("o4")
}

#[async_trait]
impl Provider for OpenAiProvider {
    fn id(&self) -> &str {
        "openai"
    }

    async fn complete(&self, req: CompleteRequest) -> anyhow::Result<CompleteResponse> {
        self.inner.complete(req).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn o_series_token_fields_switch_field_for_reasoning_models_only() {
        for model in ["o1", "o1-mini", "o3-mini", "o4-mini"] {
            assert_eq!(o_series_token_fields(model, 4096), (None, Some(4096)));
        }
        for model in ["gpt-4o-mini", "deepseek-v4-flash", "gemini-2.5-flash"] {
            assert_eq!(o_series_token_fields(model, 4096), (Some(4096), None));
        }
    }

    #[tokio::test]
    async fn openai_posts_chat_with_bearer_and_parses_text() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(header("authorization", "Bearer test-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{ "message": { "role": "assistant", "content": "hello from gpt" } }]
            })))
            .mount(&server)
            .await;

        let provider = OpenAiProvider::new(server.uri(), "test-key", "gpt-4o-mini");
        let out = provider
            .complete(CompleteRequest {
                prompt: "hi".into(),
            })
            .await
            .unwrap();
        assert_eq!(out.text, "hello from gpt");
        assert_eq!(provider.id(), "openai");
    }

    #[tokio::test]
    async fn openai_parses_usage_tokens() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{ "message": { "role": "assistant", "content": "hi" } }],
                "usage": { "prompt_tokens": 12, "completion_tokens": 34 }
            })))
            .mount(&server)
            .await;
        let provider = OpenAiProvider::new(server.uri(), "k", "gpt-4o-mini");
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
    async fn openai_surfaces_http_errors() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;

        let provider = OpenAiProvider::new(server.uri(), "bad-key", "gpt-4o-mini");
        let err = provider
            .complete(CompleteRequest {
                prompt: "hi".into(),
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("401") || err.to_string().contains("status"));
    }

    #[tokio::test]
    async fn openai_uses_max_completion_tokens_for_o_series_models() {
        use wiremock::matchers::body_partial_json;
        let server = MockServer::start().await;
        // The mock only matches if the request body contains max_completion_tokens; if the
        // provider wrongly sent max_tokens, the request would not match and the test fails.
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(body_partial_json(
                serde_json::json!({ "max_completion_tokens": 4096 }),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{ "message": { "role": "assistant", "content": "ok" } }]
            })))
            .mount(&server)
            .await;

        let provider = OpenAiProvider::new(server.uri(), "k", "o3-mini");
        let out = provider
            .complete(CompleteRequest {
                prompt: "hi".into(),
            })
            .await
            .unwrap();
        assert_eq!(out.text, "ok");
    }

    #[tokio::test]
    async fn openai_uses_max_tokens_for_gpt_models() {
        use wiremock::matchers::body_partial_json;
        let server = MockServer::start().await;
        // Symmetric to the o-series test: a gpt-* id must send `max_tokens` (not
        // `max_completion_tokens`); the mock only matches when `max_tokens` is in the body.
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(body_partial_json(serde_json::json!({ "max_tokens": 4096 })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{ "message": { "role": "assistant", "content": "ok" } }]
            })))
            .mount(&server)
            .await;

        let provider = OpenAiProvider::new(server.uri(), "k", "gpt-4o-mini");
        let out = provider
            .complete(CompleteRequest {
                prompt: "hi".into(),
            })
            .await
            .unwrap();
        assert_eq!(out.text, "ok");
    }

    #[tokio::test]
    async fn openai_returns_empty_text_for_empty_choices() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({ "choices": [] })),
            )
            .mount(&server)
            .await;

        let provider = OpenAiProvider::new(server.uri(), "k", "gpt-4o-mini");
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
