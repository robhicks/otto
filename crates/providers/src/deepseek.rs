//! `DeepSeekProvider`: talks to the DeepSeek Chat Completions API over HTTP.
//! The wire format is OpenAI-compatible, so this mirrors `OpenAiProvider`'s request/response
//! shape. Remote, requires an API key. `base_url` is configurable for testing.

use async_trait::async_trait;
use otto_engine_core::traits::Provider;
use otto_engine_core::types::{CompleteRequest, CompleteResponse};
use serde::{Deserialize, Serialize};

pub struct DeepSeekProvider {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
    max_tokens: u32,
}

impl DeepSeekProvider {
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.into(),
            api_key: api_key.into(),
            model: model.into(),
            max_tokens: 4096,
        }
    }

    /// The production API base URL. DeepSeek's OpenAI-compatible endpoint lives at
    /// `/chat/completions` on this base (the `/v1` prefix is also accepted, not versioned).
    pub fn api_base_default() -> &'static str {
        "https://api.deepseek.com"
    }
}

#[derive(Serialize)]
struct Message<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    messages: Vec<Message<'a>>,
}

#[derive(Deserialize)]
struct RespMessage {
    #[serde(default)]
    content: String,
}

#[derive(Deserialize)]
struct Choice {
    #[serde(default)]
    message: Option<RespMessage>,
}

#[derive(Deserialize)]
struct ApiUsage {
    #[serde(default)]
    prompt_tokens: u32,
    #[serde(default)]
    completion_tokens: u32,
}

#[derive(Deserialize)]
struct ChatResponse {
    #[serde(default)]
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Option<ApiUsage>,
}

#[async_trait]
impl Provider for DeepSeekProvider {
    fn id(&self) -> &str {
        "deepseek"
    }

    async fn complete(&self, req: CompleteRequest) -> anyhow::Result<CompleteResponse> {
        let url = format!("{}/chat/completions", self.base_url);
        let body = ChatRequest {
            model: &self.model,
            max_tokens: self.max_tokens,
            messages: vec![Message {
                role: "user",
                content: &req.prompt,
            }],
        };
        let resp = self
            .client
            .post(&url)
            .header("authorization", format!("Bearer {}", self.api_key))
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json::<ChatResponse>()
            .await?;
        let usage = resp.usage.as_ref().map(|u| otto_engine_core::types::Usage {
            input_tokens: u.prompt_tokens,
            output_tokens: u.completion_tokens,
        });
        let text = resp
            .choices
            .into_iter()
            .filter_map(|c| c.message)
            .map(|m| m.content)
            .collect::<Vec<_>>()
            .join("");
        Ok(CompleteResponse { text, usage })
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
