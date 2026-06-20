//! `OllamaProvider`: talks to a local Ollama server over HTTP (`/api/generate`).
//! Local, keyless. Default endpoint is `http://127.0.0.1:11434`.

use async_trait::async_trait;
use otto_engine_core::traits::Provider;
use otto_engine_core::types::{CompleteRequest, CompleteResponse};
use serde::{Deserialize, Serialize};

pub struct OllamaProvider {
    client: reqwest::Client,
    base_url: String,
    model: String,
}

impl OllamaProvider {
    /// `base_url` is the Ollama server root (no trailing slash), e.g. `http://127.0.0.1:11434`.
    pub fn new(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.into(),
            model: model.into(),
        }
    }

    /// Convenience constructor pointing at the default local Ollama endpoint.
    pub fn local_default(model: impl Into<String>) -> Self {
        Self::new("http://127.0.0.1:11434", model)
    }
}

#[derive(Serialize)]
struct GenerateRequest<'a> {
    model: &'a str,
    prompt: &'a str,
    stream: bool,
}

#[derive(Deserialize)]
struct GenerateResponse {
    response: String,
    #[serde(default)]
    prompt_eval_count: u32,
    #[serde(default)]
    eval_count: u32,
}

#[async_trait]
impl Provider for OllamaProvider {
    fn id(&self) -> &str {
        "ollama"
    }

    async fn complete(&self, req: CompleteRequest) -> anyhow::Result<CompleteResponse> {
        let url = format!("{}/api/generate", self.base_url);
        let body = GenerateRequest {
            model: &self.model,
            prompt: &req.prompt,
            stream: false,
        };
        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json::<GenerateResponse>()
            .await?;
        Ok(CompleteResponse {
            text: resp.response,
            usage: Some(otto_engine_core::types::Usage {
                input_tokens: resp.prompt_eval_count,
                output_tokens: resp.eval_count,
            }),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn ollama_posts_generate_and_parses_response() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/generate"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "response": "hello from ollama",
                "done": true
            })))
            .mount(&server)
            .await;

        let provider = OllamaProvider::new(server.uri(), "llama3.2");
        let out = provider
            .complete(CompleteRequest {
                prompt: "hi".into(),
            })
            .await
            .unwrap();
        assert_eq!(out.text, "hello from ollama");
        assert_eq!(provider.id(), "ollama");
    }

    #[tokio::test]
    async fn ollama_parses_eval_counts_as_usage() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/generate"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "response": "hi",
                "prompt_eval_count": 9,
                "eval_count": 5,
                "done": true
            })))
            .mount(&server)
            .await;
        let provider = OllamaProvider::new(server.uri(), "llama3.2");
        let out = provider
            .complete(CompleteRequest {
                prompt: "hi".into(),
            })
            .await
            .unwrap();
        assert_eq!(
            out.usage,
            Some(otto_engine_core::types::Usage {
                input_tokens: 9,
                output_tokens: 5
            })
        );
    }

    #[tokio::test]
    async fn ollama_surfaces_http_errors() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/generate"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let provider = OllamaProvider::new(server.uri(), "llama3.2");
        let err = provider
            .complete(CompleteRequest {
                prompt: "hi".into(),
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("500") || err.to_string().contains("status"));
    }
}
