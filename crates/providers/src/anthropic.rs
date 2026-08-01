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
        let base_url = base_url.into();
        Self {
            // Redirects off: this provider authenticates with `x-api-key`, which is NOT in
            // reqwest's strip list, so it would be forwarded on *any* cross-host redirect.
            client: super::base_url::build_http_client(&base_url),
            base_url,
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
        let url = super::base_url::join_url(&self.base_url, "/v1/messages");
        let body = MessagesRequest {
            model: &self.model,
            max_tokens: self.max_tokens,
            messages: vec![Message {
                role: "user",
                content: &req.prompt,
            }],
        };
        let raw = self
            .client
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .json(&body)
            .send()
            .await?;
        super::base_url::reject_redirect(&raw)?;
        let resp = raw.error_for_status()?.json::<MessagesResponse>().await?;
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

    #[tokio::test]
    async fn anthropic_does_not_follow_redirects() {
        // Redirects are disabled because reqwest's header-strip list is fixed and does not
        // include this provider's auth header (x-api-key), so a redirect would forward the
        // credential to any host. The 3xx must also be surfaced as an error rather than parsed:
        // otherwise the redirect body below would be accepted as the model's answer.
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"{"content":[{"type":"text","text":"leaked"}]}"#)
                    .insert_header("content-type", "application/json"),
            )
            .mount(&upstream)
            .await;

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header(
                        "location",
                        format!("{}/v1/messages", upstream.uri()).as_str(),
                    )
                    .insert_header("content-type", "application/json")
                    .set_body_string(r#"{"content":[{"type":"text","text":"leaked"}]}"#),
            )
            .mount(&server)
            .await;

        let provider = AnthropicProvider::new(server.uri(), "k", "claude-haiku-4-5");
        let result = provider
            .complete(CompleteRequest {
                prompt: "hi".into(),
            })
            .await;

        assert!(
            result.is_err(),
            "a 3xx must be an error, not a parsed completion; got {result:?}"
        );
        assert!(
            upstream
                .received_requests()
                .await
                .unwrap_or_default()
                .is_empty(),
            "the redirect target must never receive the credential or the prompt body"
        );
    }
}
