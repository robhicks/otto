//! `OpenAiCompatibleProvider`: the shared Chat Completions wire implementation behind both
//! `OpenAiProvider` and `DeepSeekProvider`. DeepSeek's API is OpenAI-compatible, so both talk
//! the same request/response shape; a thin per-provider shim supplies the provider `id`, the
//! base-URL path suffix, and the `max_tokens`/`max_completion_tokens` policy. Keeping the
//! wire logic in one place means a change to the shared shape (a new field, header, error
//! surface, or usage parse) is applied once, not forked.

use otto_engine_core::types::{CompleteRequest, CompleteResponse};
use serde::{Deserialize, Serialize};

/// Given a model id and the token budget, choose which (if any) of `max_tokens` /
/// `max_completion_tokens` to send. Providers with a divergent token-field convention
/// (OpenAI's o-series) supply their own; the shared default always sends `max_tokens`.
pub(crate) type TokenFields = fn(&str, u32) -> (Option<u32>, Option<u32>);

/// Always send `max_tokens` — the field DeepSeek accepts on every model, reasoning tier included.
pub(crate) fn always_max_tokens(_model: &str, budget: u32) -> (Option<u32>, Option<u32>) {
    (Some(budget), None)
}

pub(crate) struct OpenAiCompatibleProvider {
    client: reqwest::Client,
    base_url: String,
    path_suffix: &'static str,
    api_key: String,
    model: String,
    max_tokens: u32,
    token_fields: TokenFields,
}

impl OpenAiCompatibleProvider {
    pub(crate) fn new(
        base_url: String,
        path_suffix: &'static str,
        api_key: String,
        model: String,
        token_fields: TokenFields,
    ) -> Self {
        Self {
            client: super::base_url::build_http_client(&base_url),
            base_url,
            path_suffix,
            api_key,
            model,
            max_tokens: 4096,
            token_fields,
        }
    }

    pub(crate) async fn complete(&self, req: CompleteRequest) -> anyhow::Result<CompleteResponse> {
        let url = super::base_url::join_url(&self.base_url, self.path_suffix);
        let (max_tokens, max_completion_tokens) = (self.token_fields)(&self.model, self.max_tokens);
        let body = ChatRequest {
            model: &self.model,
            max_tokens,
            max_completion_tokens,
            messages: vec![Message {
                role: "user",
                content: &req.prompt,
            }],
        };
        let raw = self
            .client
            .post(&url)
            .header("authorization", format!("Bearer {}", self.api_key))
            .json(&body)
            .send()
            .await?;
        // Redirects are disabled, so a 3xx comes back as a response rather than being followed.
        // `error_for_status` only rejects 4xx/5xx, so without this a redirect body would be parsed
        // as if it were a model completion — an endpoint answering 302 with valid-looking
        // Chat-Completions JSON would have its content accepted as the answer.
        if raw.status().is_redirection() {
            anyhow::bail!(
                "provider endpoint returned {} — redirects are disabled for credential safety; \
                 point the base URL at the final endpoint instead",
                raw.status()
            );
        }
        let resp = raw.error_for_status()?.json::<ChatResponse>().await?;
        let usage = resp.usage.as_ref().map(|u| otto_engine_core::types::Usage {
            input_tokens: u.prompt_tokens,
            output_tokens: u.completion_tokens,
        });
        let text = resp
            .choices
            .into_iter()
            .filter_map(|c| c.message)
            .map(|m| m.content.unwrap_or_default())
            .collect::<Vec<_>>()
            .join("");
        Ok(CompleteResponse { text, usage })
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
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_completion_tokens: Option<u32>,
    messages: Vec<Message<'a>>,
}

#[derive(Deserialize)]
struct RespMessage {
    /// Reasoning-tier responses (e.g. DeepSeek reasoner) may carry `content: null` when only
    /// reasoning tokens were produced; treated as empty text.
    #[serde(default)]
    content: Option<String>,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn always_max_tokens_never_sends_max_completion_tokens() {
        for model in ["deepseek-v4-flash", "deepseek-reasoner"] {
            assert_eq!(always_max_tokens(model, 4096), (Some(4096), None));
        }
    }
}
