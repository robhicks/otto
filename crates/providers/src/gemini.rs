//! `GeminiProvider`: talks to the Google Gemini `generateContent` API over HTTP.
//! Remote, requires an API key. `base_url` is configurable for testing.

use async_trait::async_trait;
use otto_engine_core::traits::Provider;
use otto_engine_core::types::{CompleteRequest, CompleteResponse};
use serde::{Deserialize, Serialize};

pub struct GeminiProvider {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
    max_output_tokens: u32,
}

impl GeminiProvider {
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        let base_url = base_url.into();
        Self {
            // Redirects off: this provider authenticates with `x-goog-api-key`, which is NOT in
            // reqwest's strip list, so it would be forwarded on *any* cross-host redirect.
            client: super::base_url::build_http_client(&base_url),
            base_url,
            api_key: api_key.into(),
            model: model.into(),
            max_output_tokens: 4096,
        }
    }

    /// The production API base URL.
    pub fn api_base_default() -> &'static str {
        "https://generativelanguage.googleapis.com"
    }
}

#[derive(Serialize)]
struct Part<'a> {
    text: &'a str,
}

#[derive(Serialize)]
struct Content<'a> {
    role: &'a str,
    parts: Vec<Part<'a>>,
}

#[derive(Serialize)]
struct GenerationConfig {
    #[serde(rename = "maxOutputTokens")]
    max_output_tokens: u32,
}

#[derive(Serialize)]
struct GenerateRequest<'a> {
    contents: Vec<Content<'a>>,
    #[serde(rename = "generationConfig")]
    generation_config: GenerationConfig,
}

#[derive(Deserialize)]
struct RespPart {
    #[serde(default)]
    text: String,
}

#[derive(Deserialize)]
struct RespContent {
    #[serde(default)]
    parts: Vec<RespPart>,
}

#[derive(Deserialize)]
struct Candidate {
    #[serde(default)]
    content: Option<RespContent>,
}

#[derive(Deserialize)]
struct UsageMetadata {
    #[serde(rename = "promptTokenCount", default)]
    prompt_token_count: u32,
    #[serde(rename = "candidatesTokenCount", default)]
    candidates_token_count: u32,
}

#[derive(Deserialize)]
struct GenerateResponse {
    #[serde(default)]
    candidates: Vec<Candidate>,
    #[serde(rename = "usageMetadata", default)]
    usage_metadata: Option<UsageMetadata>,
}

#[async_trait]
impl Provider for GeminiProvider {
    fn id(&self) -> &str {
        "gemini"
    }

    async fn complete(&self, req: CompleteRequest) -> anyhow::Result<CompleteResponse> {
        let url = super::base_url::join_url(
            &self.base_url,
            &format!("/v1beta/models/{}:generateContent", self.model),
        );
        let body = GenerateRequest {
            contents: vec![Content {
                role: "user",
                parts: vec![Part { text: &req.prompt }],
            }],
            generation_config: GenerationConfig {
                max_output_tokens: self.max_output_tokens,
            },
        };
        let raw = self
            .client
            .post(&url)
            .header("x-goog-api-key", &self.api_key)
            .json(&body)
            .send()
            .await?;
        super::base_url::reject_redirect(&raw)?;
        let resp = raw.error_for_status()?.json::<GenerateResponse>().await?;
        let usage = resp
            .usage_metadata
            .as_ref()
            .map(|u| otto_engine_core::types::Usage {
                input_tokens: u.prompt_token_count,
                output_tokens: u.candidates_token_count,
            });
        let text = resp
            .candidates
            .into_iter()
            .filter_map(|c| c.content)
            .flat_map(|c| c.parts)
            .map(|p| p.text)
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
    async fn gemini_posts_generate_content_with_key_header_and_parses_text() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1beta/models/gemini-2.5-flash:generateContent"))
            .and(header("x-goog-api-key", "test-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "candidates": [
                    { "content": { "role": "model", "parts": [ { "text": "hello from gemini" } ] } }
                ]
            })))
            .mount(&server)
            .await;

        let provider = GeminiProvider::new(server.uri(), "test-key", "gemini-2.5-flash");
        let out = provider
            .complete(CompleteRequest {
                prompt: "hi".into(),
            })
            .await
            .unwrap();
        assert_eq!(out.text, "hello from gemini");
        assert_eq!(provider.id(), "gemini");
    }

    #[tokio::test]
    async fn gemini_parses_usage_tokens() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1beta/models/gemini-2.5-flash:generateContent"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "candidates": [
                    { "content": { "parts": [ { "text": "hi" } ] } }
                ],
                "usageMetadata": { "promptTokenCount": 12, "candidatesTokenCount": 34 }
            })))
            .mount(&server)
            .await;
        let provider = GeminiProvider::new(server.uri(), "k", "gemini-2.5-flash");
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
    async fn gemini_surfaces_http_errors() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1beta/models/gemini-2.5-flash:generateContent"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;

        let provider = GeminiProvider::new(server.uri(), "bad-key", "gemini-2.5-flash");
        let err = provider
            .complete(CompleteRequest {
                prompt: "hi".into(),
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("403") || err.to_string().contains("status"));
    }

    #[tokio::test]
    async fn gemini_does_not_follow_redirects() {
        // Redirects are disabled because reqwest's header-strip list is fixed and does not
        // include this provider's auth header (x-goog-api-key), so a redirect would forward the
        // credential to any host. The 3xx must also be surfaced as an error rather than parsed:
        // otherwise the redirect body below would be accepted as the model's answer.
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1beta/models/gemini-2.5-flash:generateContent"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(
                        r#"{"candidates":[{"content":{"parts":[{"text":"leaked"}]}}]}"#,
                    )
                    .insert_header("content-type", "application/json"),
            )
            .mount(&upstream)
            .await;

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1beta/models/gemini-2.5-flash:generateContent"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header(
                        "location",
                        format!(
                            "{}/v1beta/models/gemini-2.5-flash:generateContent",
                            upstream.uri()
                        )
                        .as_str(),
                    )
                    .insert_header("content-type", "application/json")
                    .set_body_string(
                        r#"{"candidates":[{"content":{"parts":[{"text":"leaked"}]}}]}"#,
                    ),
            )
            .mount(&server)
            .await;

        let provider = GeminiProvider::new(server.uri(), "k", "gemini-2.5-flash");
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
