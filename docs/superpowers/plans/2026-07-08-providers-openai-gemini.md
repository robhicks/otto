# openai + gemini Providers Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.
>
> **Commit hygiene:** Commit messages are plain — NO `Co-Authored-By`, NO "Generated with Claude", NO 🤖. This applies to every commit step below.

**Goal:** Add `OpenAiProvider` and `GeminiProvider` to the `providers` crate and generalize the engine's router wiring to select among three remote LLM providers (Anthropic/OpenAI/Gemini) via an `OTTO_REMOTE_PROVIDER` selector, per-provider keys, and portable `--model` ids — while preserving the byte-for-byte offline-deterministic default.

**Architecture:** Each new provider mirrors the existing `AnthropicProvider` one-for-one (configurable `base_url`, `wiremock`-tested, `error_for_status()` error surface). The remote-slot construction in `crates/engine/src/lib.rs` moves behind two **pure** decision helpers — `select_remote_from(...)` (default path) and `infer_remote(model)` (pinned path) — plus small env-reading wrappers, following the codebase's existing `capabilities_from_env` pure-helper convention so selection is unit-testable without mutating process-global env.

**Tech Stack:** Rust (edition 2024), `reqwest` (json + rustls-tls), `serde`/`serde_json`, `async-trait`, `wiremock` (dev), `tokio` (dev). No new dependencies.

**Reference (read before starting):**
- `docs/superpowers/specs/2026-07-08-providers-openai-gemini-design.md` — the approved design.
- `crates/providers/src/anthropic.rs` — the provider template being cloned twice.
- `crates/engine/src/lib.rs` — `build_router_with_model`, `session_config`, `capabilities_from_env`, and the existing router tests being extended.

---

## File Structure

| File | Responsibility |
|---|---|
| `crates/providers/src/openai.rs` | **new** — `OpenAiProvider` (Chat Completions) + 3 wiremock tests |
| `crates/providers/src/gemini.rs` | **new** — `GeminiProvider` (`generateContent`) + 3 wiremock tests |
| `crates/providers/src/lib.rs` | **modify** — declare + export the two new modules |
| `crates/engine/src/lib.rs` | **modify** — `RemoteChoice` + selection/inference/build helpers + `DEFAULT_OPENAI_MODEL`/`DEFAULT_GEMINI_MODEL`; rewrite `build_router_with_model`; widen `capabilities_from_env`/`build_capabilities`; generalize `session_config`; new unit tests |

---

## Task 1: `OpenAiProvider`

**Files:**
- Create: `crates/providers/src/openai.rs`
- Modify: `crates/providers/src/lib.rs`

- [ ] **Step 1: Create the provider + its three failing tests**

Create `crates/providers/src/openai.rs` with the full content below (implementation + tests together — the tests reference the type, so they compile as a unit):

```rust
//! `OpenAiProvider`: talks to the OpenAI Chat Completions API over HTTP.
//! Remote, requires an API key. `base_url` is configurable for testing.

use async_trait::async_trait;
use otto_engine_core::traits::Provider;
use otto_engine_core::types::{CompleteRequest, CompleteResponse};
use serde::{Deserialize, Serialize};

pub struct OpenAiProvider {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
    max_tokens: u32,
}

impl OpenAiProvider {
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
        "https://api.openai.com"
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
impl Provider for OpenAiProvider {
    fn id(&self) -> &str {
        "openai"
    }

    async fn complete(&self, req: CompleteRequest) -> anyhow::Result<CompleteResponse> {
        let url = format!("{}/v1/chat/completions", self.base_url);
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
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

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
}
```

- [ ] **Step 2: Wire the module into the crate**

In `crates/providers/src/lib.rs`, add the module declaration and export. The file currently reads:

```rust
pub mod anthropic;
pub mod local;
pub mod ollama;
pub mod scripted;

pub use anthropic::AnthropicProvider;
pub use local::LocalProvider;
pub use ollama::OllamaProvider;
pub use scripted::ScriptedProvider;
```

Change it to (adds the `openai` lines; `gemini` is added in Task 2):

```rust
pub mod anthropic;
pub mod local;
pub mod ollama;
pub mod openai;
pub mod scripted;

pub use anthropic::AnthropicProvider;
pub use local::LocalProvider;
pub use ollama::OllamaProvider;
pub use openai::OpenAiProvider;
pub use scripted::ScriptedProvider;
```

- [ ] **Step 3: Run the OpenAI tests — expect PASS**

Run: `cargo test -p otto-providers openai::`
Expected: 3 tests pass (`openai_posts_chat_with_bearer_and_parses_text`, `openai_parses_usage_tokens`, `openai_surfaces_http_errors`).

(These are written test-first but pass immediately because the implementation ships in the same step — the "failing" state would only appear if the impl were omitted. If any test fails, the impl is wrong; fix `openai.rs` before proceeding.)

- [ ] **Step 4: Format**

Run: `cargo fmt -p otto-providers`
Expected: no diff or a clean reformat; re-run `cargo test -p otto-providers openai::` to confirm still green.

- [ ] **Step 5: Commit**

```bash
git add crates/providers/src/openai.rs crates/providers/src/lib.rs
git commit -m "feat(providers): OpenAiProvider over Chat Completions"
```

---

## Task 2: `GeminiProvider`

**Files:**
- Create: `crates/providers/src/gemini.rs`
- Modify: `crates/providers/src/lib.rs`

- [ ] **Step 1: Create the provider + its three failing tests**

Create `crates/providers/src/gemini.rs` with the full content below:

```rust
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
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.into(),
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
        let url = format!(
            "{}/v1beta/models/{}:generateContent",
            self.base_url, self.model
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
        let resp = self
            .client
            .post(&url)
            .header("x-goog-api-key", &self.api_key)
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json::<GenerateResponse>()
            .await?;
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
}
```

- [ ] **Step 2: Wire the module into the crate**

In `crates/providers/src/lib.rs`, add the `gemini` module and export so the file reads:

```rust
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
```

- [ ] **Step 3: Run the Gemini tests — expect PASS**

Run: `cargo test -p otto-providers gemini::`
Expected: 3 tests pass (`gemini_posts_generate_content_with_key_header_and_parses_text`, `gemini_parses_usage_tokens`, `gemini_surfaces_http_errors`).

- [ ] **Step 4: Format + full crate test**

Run: `cargo fmt -p otto-providers && cargo test -p otto-providers`
Expected: the whole `providers` crate is green (all existing anthropic/local/ollama/scripted tests plus the 6 new ones).

- [ ] **Step 5: Commit**

```bash
git add crates/providers/src/gemini.rs crates/providers/src/lib.rs
git commit -m "feat(providers): GeminiProvider over generateContent"
```

---

## Task 3: Generalize router selection to three remotes

This task adds the selection seams **and** rewrites `build_router_with_model` to use them in one cohesive change, so the build stays green (the helpers are consumed immediately — no dead code) and the existing determinism tests keep passing.

**Files:**
- Modify: `crates/engine/src/lib.rs`

- [ ] **Step 1: Add the pure-helper unit tests (failing)**

In `crates/engine/src/lib.rs`, inside the existing `#[cfg(test)] mod tests { ... }` block (the one that already holds `default_build_router_is_offline_and_deterministic`), add these tests. They reference `RemoteChoice`, `select_remote_from`, and `infer_remote`, which don't exist yet, so the test module will fail to compile — that is the expected "red" state.

```rust
    #[test]
    fn select_remote_from_precedence_when_no_selector() {
        // Precedence: Anthropic > OpenAi > Gemini among present keys.
        assert_eq!(
            select_remote_from(None, true, true, true),
            Some(RemoteChoice::Anthropic)
        );
        assert_eq!(
            select_remote_from(None, false, true, true),
            Some(RemoteChoice::OpenAi)
        );
        assert_eq!(
            select_remote_from(None, false, false, true),
            Some(RemoteChoice::Gemini)
        );
        assert_eq!(select_remote_from(None, false, false, false), None);
    }

    #[test]
    fn select_remote_from_selector_wins_when_its_key_present() {
        // A valid selector overrides precedence even when a higher-precedence key exists.
        assert_eq!(
            select_remote_from(Some("openai"), true, true, true),
            Some(RemoteChoice::OpenAi)
        );
        assert_eq!(
            select_remote_from(Some("gemini"), true, false, true),
            Some(RemoteChoice::Gemini)
        );
        // Case-insensitive.
        assert_eq!(
            select_remote_from(Some("OpenAI"), false, true, false),
            Some(RemoteChoice::OpenAi)
        );
    }

    #[test]
    fn select_remote_from_selector_without_key_is_none() {
        // Selector names a provider whose key is absent -> None (offline), NOT a fallback to
        // another provider's key.
        assert_eq!(select_remote_from(Some("openai"), true, false, true), None);
        assert_eq!(select_remote_from(Some("gemini"), true, true, false), None);
    }

    #[test]
    fn select_remote_from_unknown_selector_falls_through_to_precedence() {
        assert_eq!(
            select_remote_from(Some("bogus"), true, false, false),
            Some(RemoteChoice::Anthropic)
        );
        assert_eq!(select_remote_from(Some("bogus"), false, false, false), None);
    }

    #[test]
    fn infer_remote_maps_model_id_prefixes() {
        assert_eq!(infer_remote("gpt-4o"), Some(RemoteChoice::OpenAi));
        assert_eq!(infer_remote("gpt-4o-mini"), Some(RemoteChoice::OpenAi));
        assert_eq!(infer_remote("o1-preview"), Some(RemoteChoice::OpenAi));
        assert_eq!(infer_remote("o3-mini"), Some(RemoteChoice::OpenAi));
        assert_eq!(infer_remote("gemini-2.5-pro"), Some(RemoteChoice::Gemini));
        assert_eq!(infer_remote("claude-opus-4-8"), Some(RemoteChoice::Anthropic));
        assert_eq!(infer_remote("llama3.2"), None);
        assert_eq!(infer_remote("mistral-large"), None);
    }
```

- [ ] **Step 2: Run the new tests — expect compile FAIL**

Run: `cargo test -p otto-engine select_remote_from 2>&1 | tail -20`
Expected: compilation error — `cannot find value/function 'select_remote_from'`, `cannot find type 'RemoteChoice'`, `cannot find function 'infer_remote'`.

- [ ] **Step 3: Add `RemoteChoice` + the helpers, and update imports/constants**

In `crates/engine/src/lib.rs`:

(a) Update the providers import (currently `use otto_providers::{AnthropicProvider, LocalProvider, OllamaProvider};`) to:

```rust
use otto_providers::{
    AnthropicProvider, GeminiProvider, LocalProvider, OllamaProvider, OpenAiProvider,
};
```

(b) Below the existing default-model constants (`DEFAULT_OLLAMA_MODEL` / `DEFAULT_ANTHROPIC_MODEL`), add:

```rust
const DEFAULT_OPENAI_MODEL: &str = "gpt-4o-mini";
const DEFAULT_GEMINI_MODEL: &str = "gemini-2.5-flash";
```

(c) Add the choice enum + helpers. Place them immediately above `build_router` (after `build_local_provider`):

```rust
/// Which remote provider the router's single remote slot uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemoteChoice {
    Anthropic,
    OpenAi,
    Gemini,
}

impl RemoteChoice {
    /// Stable id recorded in `session_config` and matching each provider's `Provider::id()`.
    fn id(self) -> &'static str {
        match self {
            RemoteChoice::Anthropic => "anthropic",
            RemoteChoice::OpenAi => "openai",
            RemoteChoice::Gemini => "gemini",
        }
    }
}

/// Pure remote-slot selection for the default (non-pinned) path. Takes explicit inputs so it
/// is unit-testable without mutating process-global env (mirrors `capabilities_from_env`).
///
/// `selector` is the raw `OTTO_REMOTE_PROVIDER` value; the three bools are "this provider's key
/// is present and non-empty". A valid selector wins when its key is present; a selector whose
/// key is absent yields `None` (offline) rather than silently falling back to another
/// provider; an unknown selector is ignored and precedence applies:
/// Anthropic > OpenAI > Gemini.
fn select_remote_from(
    selector: Option<&str>,
    anthropic: bool,
    openai: bool,
    gemini: bool,
) -> Option<RemoteChoice> {
    if let Some(sel) = selector {
        match sel.to_ascii_lowercase().as_str() {
            "anthropic" => {
                return present_or_warn(anthropic, RemoteChoice::Anthropic, "ANTHROPIC_API_KEY");
            }
            "openai" => return present_or_warn(openai, RemoteChoice::OpenAi, "OPENAI_API_KEY"),
            "gemini" => return present_or_warn(gemini, RemoteChoice::Gemini, "GEMINI_API_KEY"),
            other => {
                eprintln!(
                    "warning: OTTO_REMOTE_PROVIDER='{other}' is not a known provider \
                     (anthropic|openai|gemini); using key precedence instead"
                );
            }
        }
    }
    if anthropic {
        Some(RemoteChoice::Anthropic)
    } else if openai {
        Some(RemoteChoice::OpenAi)
    } else if gemini {
        Some(RemoteChoice::Gemini)
    } else {
        None
    }
}

/// Helper for `select_remote_from`: return the choice if its key is present, else warn and
/// select nothing (offline) — a named-but-unusable selector must not misroute to another key.
fn present_or_warn(present: bool, choice: RemoteChoice, key: &str) -> Option<RemoteChoice> {
    if present {
        Some(choice)
    } else {
        eprintln!(
            "warning: OTTO_REMOTE_PROVIDER='{}' but {key} is not set; \
             falling back to the offline/local router",
            choice.id()
        );
        None
    }
}

/// Pure model-id -> provider inference for the pinned path. Returns `None` for ids that do not
/// match a known provider prefix (the caller then uses the active remote, if any).
fn infer_remote(model: &str) -> Option<RemoteChoice> {
    if model.starts_with("gpt-")
        || model.starts_with("o1")
        || model.starts_with("o3")
        || model.starts_with("o4")
    {
        Some(RemoteChoice::OpenAi)
    } else if model.starts_with("gemini-") {
        Some(RemoteChoice::Gemini)
    } else if model.starts_with("claude-") {
        Some(RemoteChoice::Anthropic)
    } else {
        None
    }
}

/// True when the given provider's API key is present and non-empty in the environment.
fn has_key(choice: RemoteChoice) -> bool {
    let var = match choice {
        RemoteChoice::Anthropic => "ANTHROPIC_API_KEY",
        RemoteChoice::OpenAi => "OPENAI_API_KEY",
        RemoteChoice::Gemini => "GEMINI_API_KEY",
    };
    std::env::var(var).map(|k| !k.is_empty()).unwrap_or(false)
}

/// Env-reading wrapper over `select_remote_from` — the default-path remote selection.
fn select_remote() -> Option<RemoteChoice> {
    select_remote_from(
        std::env::var("OTTO_REMOTE_PROVIDER").ok().as_deref(),
        has_key(RemoteChoice::Anthropic),
        has_key(RemoteChoice::OpenAi),
        has_key(RemoteChoice::Gemini),
    )
}

/// The effective default model for a provider (its `OTTO_<P>_MODEL` env var, else the constant).
fn default_model_for(choice: RemoteChoice) -> String {
    match choice {
        RemoteChoice::Anthropic => std::env::var("OTTO_ANTHROPIC_MODEL")
            .unwrap_or_else(|_| DEFAULT_ANTHROPIC_MODEL.to_string()),
        RemoteChoice::OpenAi => {
            std::env::var("OTTO_OPENAI_MODEL").unwrap_or_else(|_| DEFAULT_OPENAI_MODEL.to_string())
        }
        RemoteChoice::Gemini => {
            std::env::var("OTTO_GEMINI_MODEL").unwrap_or_else(|_| DEFAULT_GEMINI_MODEL.to_string())
        }
    }
}

/// Construct the remote provider for `choice`, pinned to `model`. Callers must have confirmed
/// the provider's key is present (`has_key`/`select_remote`); the key is read here.
fn build_remote(choice: RemoteChoice, model: String) -> Arc<dyn Provider> {
    match choice {
        RemoteChoice::Anthropic => Arc::new(AnthropicProvider::new(
            AnthropicProvider::api_base_default(),
            std::env::var("ANTHROPIC_API_KEY").unwrap_or_default(),
            model,
        )),
        RemoteChoice::OpenAi => {
            let base = std::env::var("OPENAI_BASE_URL")
                .unwrap_or_else(|_| OpenAiProvider::api_base_default().to_string());
            Arc::new(OpenAiProvider::new(
                base,
                std::env::var("OPENAI_API_KEY").unwrap_or_default(),
                model,
            ))
        }
        RemoteChoice::Gemini => Arc::new(GeminiProvider::new(
            GeminiProvider::api_base_default(),
            std::env::var("GEMINI_API_KEY").unwrap_or_default(),
            model,
        )),
    }
}
```

- [ ] **Step 4: Rewrite `build_router_with_model` to use the seams**

Replace the entire `build_router_with_model` function (its doc comment and body) with:

```rust
/// Build a router, optionally pinning the remote slot to an explicit model id (from a
/// command/agent `model:` field).
///
/// - `model_override = None`: select a remote via [`select_remote`] (an `OTTO_REMOTE_PROVIDER`
///   selector, else key precedence Anthropic > OpenAI > Gemini). Some -> `BrainBlendRouter`
///   over that provider at its default model; None -> the offline `SingleProviderRouter`.
/// - `model_override = Some(m)`: infer the provider from `m`'s prefix ([`infer_remote`]); an
///   unrecognized prefix uses the active remote from [`select_remote`]. If the chosen
///   provider's key is present, build it pinned to `m` in a `PinnedModelRouter`; otherwise warn
///   and fall back to the offline `SingleProviderRouter` (keeping the default deterministic).
///
/// With no provider keys and no selector set, both branches yield
/// `SingleProviderRouter(LocalProvider)` — the byte-for-byte offline-deterministic default.
pub fn build_router_with_model(model_override: Option<&str>) -> Box<dyn otto_engine_core::Router> {
    let local = build_local_provider();

    match model_override {
        Some(model) => {
            // Known prefix is authoritative (its own key required); unknown prefix uses the
            // active remote. Either way the chosen provider's key must be present.
            let choice = infer_remote(model).or_else(select_remote);
            match choice.filter(|c| has_key(*c)) {
                Some(c) => {
                    let remote = build_remote(c, model.to_string());
                    Box::new(PinnedModelRouter::new(local, remote))
                }
                None => {
                    eprintln!(
                        "warning: requested model '{model}' but no usable provider key is set; \
                         falling back to the offline/local router"
                    );
                    Box::new(SingleProviderRouter::new(local))
                }
            }
        }
        None => match select_remote() {
            Some(c) => {
                let remote = build_remote(c, default_model_for(c));
                Box::new(BrainBlendRouter::new(local, remote))
            }
            None => Box::new(SingleProviderRouter::new(local)),
        },
    }
}
```

- [ ] **Step 5: Run the new + existing router tests — expect PASS**

Run: `cargo test -p otto-engine select_remote_from infer_remote 2>&1 | tail -20`
Expected: the 5 new pure-helper tests pass.

Run: `cargo test -p otto-engine build_router default_build_router_is_offline model_override_without_key 2>&1 | tail -20`
Expected: the two pre-existing determinism tests still pass (offline default unchanged; pinned-without-key still offline).

- [ ] **Step 6: Format + clippy on the crate**

Run: `cargo fmt -p otto-engine && cargo clippy -p otto-engine --all-targets 2>&1 | tail -20`
Expected: no errors. (If clippy flags `has_key(*c)` in the `.filter` closure, it is correct as written — `filter` passes `&RemoteChoice`; leave it.)

- [ ] **Step 7: Commit**

```bash
git add crates/engine/src/lib.rs
git commit -m "feat(engine): route to anthropic/openai/gemini via selector + model-id inference"
```

---

## Task 4: Widen capabilities + generalize `session_config`

Move the two remaining Anthropic-specific env checks onto the `select_remote()` seam so a session served with only an OpenAI or Gemini key correctly reports and records a remote LLM.

**Files:**
- Modify: `crates/engine/src/lib.rs`

- [ ] **Step 1: Update the `capabilities_from_env` pure test (failing)**

The existing `capabilities_from_env_maps_flags` test passes `anthropic_key: Option<&str>` as the middle argument. Change the middle parameter to a `remote_llm: bool`. Replace the body of `capabilities_from_env_maps_flags` with:

```rust
    #[test]
    fn capabilities_from_env_maps_flags() {
        // Pure mapping — takes raw inputs, touches no process-global env, so it does NOT race
        // the env-reading router test in this same binary.
        // Nothing set → fully offline, local engine, no sandbox.
        assert_eq!(
            capabilities_from_env(None, false, false),
            CapabilitiesManifest {
                engine_remote: false,
                local_llm: false,
                remote_llm: false,
                sandbox: false,
            }
        );
        // OTTO_OLLAMA must equal exactly "1" to count as a local LLM.
        assert!(capabilities_from_env(Some("1"), false, false).local_llm);
        assert!(!capabilities_from_env(Some("0"), false, false).local_llm);
        // remote_llm now reflects "a remote provider is selectable" (any of the three keys /
        // a valid selector), computed by the caller via select_remote().is_some().
        assert!(capabilities_from_env(None, true, false).remote_llm);
        assert!(!capabilities_from_env(None, false, false).remote_llm);
        // sandbox passes through unchanged.
        assert!(capabilities_from_env(None, false, true).sandbox);
    }
```

- [ ] **Step 2: Run it — expect compile FAIL**

Run: `cargo test -p otto-engine capabilities_from_env_maps_flags 2>&1 | tail -15`
Expected: compile error — `capabilities_from_env` still takes `Option<&str>` for the middle arg, so `capabilities_from_env(None, true, false)` (a `bool`) mismatches.

- [ ] **Step 3: Change `capabilities_from_env`'s signature**

Replace the `capabilities_from_env` function with (middle param is now a `bool`):

```rust
fn capabilities_from_env(
    otto_ollama: Option<&str>,
    remote_llm: bool,
    sandbox: bool,
) -> CapabilitiesManifest {
    CapabilitiesManifest {
        engine_remote: false,
        local_llm: otto_ollama == Some("1"),
        remote_llm,
        sandbox,
    }
}
```

- [ ] **Step 4: Update `build_capabilities` to compute the bool via `select_remote`**

Replace the body of `build_capabilities` with:

```rust
pub fn build_capabilities() -> CapabilitiesManifest {
    capabilities_from_env(
        std::env::var("OTTO_OLLAMA").ok().as_deref(),
        select_remote().is_some(),
        os_sandbox_available(),
    )
}
```

- [ ] **Step 5: Generalize `session_config`**

Replace the `session_config` function with (records the resolved remote provider + model instead of Anthropic-specific fields; `ollama` recording unchanged):

```rust
/// Snapshot the provider-selection environment into JSON for a session's `config` column.
/// Mirrors the env that `build_router` reads (re-running remote selection so the record matches
/// the routing), so a stored session records which backends it was configured to use. Lives in
/// the wiring layer (not core) because it reads `OTTO_*` / provider API keys.
pub fn session_config() -> serde_json::Value {
    let ollama = std::env::var("OTTO_OLLAMA").as_deref() == Ok("1");
    let remote = select_remote();
    serde_json::json!({
        "ollama": ollama,
        "remote": remote.is_some(),
        // The resolved remote provider id ("anthropic"|"openai"|"gemini") or "none".
        "remote_provider": remote.map(RemoteChoice::id).unwrap_or("none"),
        // Record the EFFECTIVE models (the build_router defaults when the env vars are unset),
        // so a restored session's config reflects the routing it actually used.
        "ollama_model": std::env::var("OTTO_OLLAMA_MODEL")
            .unwrap_or_else(|_| DEFAULT_OLLAMA_MODEL.to_string()),
        "remote_model": remote.map(default_model_for).unwrap_or_else(|| "none".to_string()),
    })
}
```

- [ ] **Step 6: Run the affected tests — expect PASS**

Run: `cargo test -p otto-engine capabilities_from_env_maps_flags 2>&1 | tail -15`
Expected: PASS.

Run: `cargo test -p otto-engine 2>&1 | tail -25`
Expected: the whole `otto-engine` crate (lib + integration tests, including `tests/serve.rs`) is green. In particular `tests/serve.rs`'s `caps["remote_llm"] == false` assertion still holds because that test sets no provider keys, so `select_remote()` is `None`.

- [ ] **Step 7: Commit**

```bash
git add crates/engine/src/lib.rs
git commit -m "feat(engine): report/record remote LLM for any of anthropic/openai/gemini"
```

---

## Task 5: Full-workspace verification

**Files:** none (verification only).

- [ ] **Step 1: Format the whole workspace**

Run: `cargo fmt --all`
Expected: no diff (everything already formatted per prior tasks).

- [ ] **Step 2: Clippy the whole workspace**

Run: `cargo clippy --workspace --all-targets 2>&1 | tail -30`
Expected: no warnings or errors.

- [ ] **Step 3: Run the full test suite (offline/deterministic)**

Run: `cargo test --workspace 2>&1 | tail -40`
Expected: all crates green. This proves the offline-determinism invariant holds — CI sets none of `OTTO_OLLAMA`/`ANTHROPIC_API_KEY`/`OPENAI_API_KEY`/`GEMINI_API_KEY`/`OTTO_REMOTE_PROVIDER`, so `select_remote()` is `None` and routing is `SingleProviderRouter(LocalProvider)` exactly as before.

- [ ] **Step 4: Spec-coverage self-check (no code)**

Confirm each design requirement maps to shipped work:
- `OpenAiProvider` (Chat Completions, Bearer, `OPENAI_BASE_URL` override, wiremock-tested) — Task 1 + Task 3(build_remote).
- `GeminiProvider` (`generateContent`, `x-goog-api-key`, wiremock-tested) — Task 2.
- `select_remote` (selector + precedence) and `infer_remote` (model-id prefix), pure + unit-tested — Task 3.
- Defaults `gpt-4o-mini` / `gemini-2.5-flash` — Task 3.
- `remote_llm` widened + `session_config` generalized — Task 4.
- Offline determinism preserved — Task 5 Step 3.

If everything above is checked, the plan is complete. No final commit needed (Step 1 produces no diff); if `cargo fmt --all` did change anything, commit it with `git commit -am "style: cargo fmt"`.
```
