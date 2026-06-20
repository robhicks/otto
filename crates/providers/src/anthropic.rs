//! `AnthropicProvider`: talks to the Anthropic Messages API over HTTP.
//! Remote, requires an API key. `base_url` is configurable for testing.

use async_trait::async_trait;
use otto_engine_core::traits::Provider;
use otto_engine_core::types::{CompleteRequest, CompleteResponse};
use serde::{Deserialize, Serialize};

const ANTHROPIC_VERSION: &str = "2023-06-01";

pub struct AnthropicProvider {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
    max_tokens: u32,
}

impl AnthropicProvider {
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

    /// The production API base URL.
    pub fn api_base_default() -> &'static str {
        "https://api.anthropic.com"
    }
}

#[derive(Serialize)]
struct Message<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Serialize)]
struct MessagesRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    messages: Vec<Message<'a>>,
}

#[derive(Deserialize)]
struct ContentBlock {
    #[serde(default)]
    text: String,
}

#[derive(Deserialize)]
struct ApiUsage {
    #[serde(default)]
    input_tokens: u32,
    #[serde(default)]
    output_tokens: u32,
}

#[derive(Deserialize)]
struct MessagesResponse {
    content: Vec<ContentBlock>,
    #[serde(default)]
    usage: Option<ApiUsage>,
}

#[async_trait]
impl Provider for AnthropicProvider {
    fn id(&self) -> &str {
        "anthropic"
    }

    async fn complete(&self, req: CompleteRequest) -> anyhow::Result<CompleteResponse> {
        let url = format!("{}/v1/messages", self.base_url);
        let body = MessagesRequest {
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
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json::<MessagesResponse>()
            .await?;
        let usage = resp.usage.as_ref().map(|u| otto_engine_core::types::Usage {
            input_tokens: u.input_tokens,
            output_tokens: u.output_tokens,
        });
        let text = resp
            .content
            .into_iter()
            .map(|b| b.text)
            .collect::<Vec<_>>()
            .join("");
        Ok(CompleteResponse { text, usage })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn anthropic_posts_messages_with_headers_and_parses_text() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("x-api-key", "test-key"))
            .and(header("anthropic-version", ANTHROPIC_VERSION))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "content": [{ "type": "text", "text": "hello from claude" }]
            })))
            .mount(&server)
            .await;

        let provider = AnthropicProvider::new(server.uri(), "test-key", "claude-haiku-4-5");
        let out = provider
            .complete(CompleteRequest {
                prompt: "hi".into(),
            })
            .await
            .unwrap();
        assert_eq!(out.text, "hello from claude");
        assert_eq!(provider.id(), "anthropic");
    }

    #[tokio::test]
    async fn anthropic_parses_usage_tokens() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "content": [{ "type": "text", "text": "hi" }],
                "usage": { "input_tokens": 12, "output_tokens": 34 }
            })))
            .mount(&server)
            .await;
        let provider = AnthropicProvider::new(server.uri(), "k", "claude-haiku-4-5");
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
    async fn anthropic_surfaces_http_errors() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;

        let provider = AnthropicProvider::new(server.uri(), "bad-key", "claude-haiku-4-5");
        let err = provider
            .complete(CompleteRequest {
                prompt: "hi".into(),
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("401") || err.to_string().contains("status"));
    }
}
