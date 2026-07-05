//! DeepSeek API client.
//!
//! Thin wrapper around the OpenAI-compatible chat completions endpoint:
//! `POST https://api.deepseek.com/chat/completions`. The client is constructed
//! once with the API key read from `.env` (`DEEPSEEK_API_KEY`) and shared via
//! `web::Data`.
//!
//! This module is the **only** place in the backend that knows about the
//! DeepSeek wire format. Handlers translate domain prompts into
//! [`DeepSeekMessage`]s, call [`DeepSeekClient::chat_completion`], and read
//! the structured response. Future Socratic-dialogue and Feynman-evaluation
//! handlers should reuse this client rather than re-implementing HTTP.

use serde::{Deserialize, Serialize};

/// Default model — verified working in the M1-closeout connectivity test
/// (`scripts/test-deepseek.sh`) and per the DeepSeek API docs (May 2026). The
/// legacy `deepseek-chat` alias is scheduled for retirement on 2026-07-24.
pub const DEFAULT_MODEL: &str = "deepseek-v4-flash";

/// Base URL — same as the working shell-script test, no trailing slash.
const BASE_URL: &str = "https://api.deepseek.com";

/// A chat message in the OpenAI/DeepSeek wire format. Either role can be
/// serialized by the client; callers normally send `system` + `user`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeepSeekMessage {
    pub role: &'static str,
    pub content: String,
}

impl DeepSeekMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self { role: "system", content: content.into() }
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self { role: "user", content: content.into() }
    }
}

#[derive(Debug, Serialize)]
struct ChatCompletionsRequest<'a> {
    model: &'a str,
    messages: &'a [DeepSeekMessage],
    stream: bool,
}

/// Subset of the response we care about. The full schema has many fields
/// (`reasoning_content`, `prompt_cache_hit_tokens`, etc.) that we ignore.
#[derive(Debug, Deserialize)]
pub struct DeepSeekResponse {
    /// `choices[0].message.content` — the model's text response.
    pub choices: Vec<DeepSeekChoice>,
    pub usage: DeepSeekUsage,
}

#[derive(Debug, Deserialize)]
pub struct DeepSeekChoice {
    pub message: DeepSeekChoiceMessage,
}

#[derive(Debug, Deserialize)]
pub struct DeepSeekChoiceMessage {
    pub content: String,
}

#[derive(Debug, Deserialize, Default)]
pub struct DeepSeekUsage {
    /// Total tokens across prompt + completion. Used for `ai_interactions.tokens_used`.
    #[serde(default)]
    pub total_tokens: u32,
    /// Prompt-side token count. Kept for future cost breakdown; currently unused.
    #[serde(default)]
    #[allow(dead_code)]
    pub prompt_tokens: u32,
    /// Completion-side token count. Kept for future cost breakdown; currently unused.
    #[serde(default)]
    #[allow(dead_code)]
    pub completion_tokens: u32,
}

/// Errors from the DeepSeek client. All variants keep the API key out of any
/// `Display`/`Debug` rendering by construction (we never store the key in
/// error fields).
#[derive(Debug)]
pub enum DeepSeekError {
    /// reqwest failed before/while sending (network, TLS, DNS).
    Network(String),
    /// The upstream returned a non-200 status. `body` is the response body
    /// (free of the API key — DeepSeek does not echo the bearer token in
    /// error bodies).
    Http { status: u16, body: String },
    /// The response body could not be parsed as the expected schema.
    Parse(String),
}

impl std::fmt::Display for DeepSeekError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DeepSeekError::Network(m) => write!(f, "network error: {m}"),
            DeepSeekError::Http { status, body } => {
                // Truncate the body so a verbose upstream error doesn't blow
                // up our `ai_interactions.output_text` column or logs.
                let snippet: String = body.chars().take(500).collect();
                write!(f, "upstream HTTP {status}: {snippet}")
            }
            DeepSeekError::Parse(m) => write!(f, "parse error: {m}"),
        }
    }
}

impl std::error::Error for DeepSeekError {}

/// HTTP client wrapping reqwest. Construct once with the API key.
pub struct DeepSeekClient {
    http: reqwest::Client,
    api_key: String,
}

impl DeepSeekClient {
    /// Construct a new client. The API key is read from the env var
    /// `DEEPSEEK_API_KEY`. Returns `None` if the env var is unset — call at
    /// startup so the absence fails fast with a clear message.
    pub fn from_env() -> Option<Self> {
        let api_key = std::env::var("DEEPSEEK_API_KEY").ok()?;
        if api_key.is_empty() {
            return None;
        }
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .expect("reqwest client construction should not fail with sane defaults");
        Some(Self { http, api_key })
    }

    /// Send a chat-completions request and return the structured response.
    ///
    /// `model` may be `None` to use [`DEFAULT_MODEL`].
    pub async fn chat_completion(
        &self,
        messages: &[DeepSeekMessage],
        model: Option<&str>,
    ) -> Result<DeepSeekResponse, DeepSeekError> {
        let req = ChatCompletionsRequest {
            model: model.unwrap_or(DEFAULT_MODEL),
            messages,
            stream: false,
        };

        let resp = self
            .http
            .post(format!("{BASE_URL}/chat/completions"))
            .bearer_auth(&self.api_key)
            .json(&req)
            .send()
            .await
            .map_err(|e| DeepSeekError::Network(e.to_string()))?;

        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(DeepSeekError::Http {
                status: status.as_u16(),
                body,
            });
        }

        serde_json::from_str::<DeepSeekResponse>(&body)
            .map_err(|e| DeepSeekError::Parse(format!("{e}; body snippet: {}", body.chars().take(500).collect::<String>())))
    }
}