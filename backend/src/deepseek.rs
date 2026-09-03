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
use crate::llm_provider::{LLMMessage, LLMResponse, LLMError, LLMProvider};
use async_trait::async_trait;

/// Default model — verified working in the M1-closeout connectivity test
/// (`scripts/test-deepseek.sh`) and per the DeepSeek API docs (May 2026). The
/// legacy `deepseek-chat` alias is scheduled for retirement on 2026-07-24.
pub const DEFAULT_MODEL: &str = "deepseek-v4-flash";

/// Base URL — same as the working shell-script test, no trailing slash.
const BASE_URL: &str = "https://api.deepseek.com";

/// The `finish_reason` value meaning the model ran out of token budget before
/// finishing its answer.
const FINISH_REASON_LENGTH: &str = "length";

/// A chat message in the OpenAI/DeepSeek wire format. Either role can be
/// serialized by the client; callers normally send `system` + `user`.
///
/// Note: Handlers build messages via [`crate::llm_provider::LLMMessage`] and the
/// provider adapter converts to this struct at the wire boundary. No direct
/// constructor is exposed — construct one with struct literal syntax if you
/// need a raw DeepSeekMessage for debugging/low-level tests.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeepSeekMessage {
    pub role: &'static str,
    pub content: String,
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
    /// Why generation stopped: `"stop"` normally, `"length"` when the token
    /// budget ran out mid-answer.
    ///
    /// Optional on the wire rather than required: a missing field must not
    /// turn an otherwise fine response into a parse error, since we only act
    /// on one specific value. Absent is treated as "not truncated" — see
    /// [`finish_reason_of`].
    #[serde(default)]
    pub finish_reason: Option<String>,
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

/// The stop reason reported for the first choice, or `""` if the field was
/// absent. Only the first choice matters — we never request more than one.
fn finish_reason_of(resp: &DeepSeekResponse) -> &str {
    resp.choices
        .first()
        .and_then(|c| c.finish_reason.as_deref())
        .unwrap_or_default()
}

/// Translate a parsed DeepSeek response into the provider-agnostic
/// [`LLMResponse`], **rejecting a truncated completion**.
///
/// The rejection is the point of this function, and it lives here — at the one
/// place every provider call funnels through — rather than at each handler, so
/// that a new call site cannot forget it. Truncation is otherwise silent:
/// reasoning tokens are billed against `max_tokens` without ever appearing in
/// the output, so an answer cut short arrives looking like an ordinary
/// response that merely happens to be empty or to stop mid-sentence. Handing
/// that to a JSON parser reports a formatting problem, which sends whoever
/// reads the error looking in the wrong place entirely.
fn to_llm_response(resp: DeepSeekResponse) -> Result<LLMResponse, LLMError> {
    let finish_reason = finish_reason_of(&resp).to_string();
    if finish_reason == FINISH_REASON_LENGTH {
        return Err(LLMError::Truncated { finish_reason });
    }

    let content = resp
        .choices
        .first()
        .map(|c| c.message.content.clone())
        .unwrap_or_default();

    Ok(LLMResponse {
        content,
        total_tokens: resp.usage.total_tokens,
        finish_reason,
    })
}

#[async_trait]
impl LLMProvider for DeepSeekClient {
    async fn chat_completion(
        &self,
        messages: &[LLMMessage],
        model: Option<&str>,
    ) -> Result<LLMResponse, LLMError> {
        let ds_messages: Vec<DeepSeekMessage> = messages
            .iter()
            .map(|m| DeepSeekMessage {
                role: m.role,
                content: m.content.clone(),
            })
            .collect();

        let resp = self
            .chat_completion(&ds_messages, model)
            .await
            .map_err(|e| match e {
                DeepSeekError::Network(m) => LLMError::Network(m),
                DeepSeekError::Http { status, body } => LLMError::Http { status, body },
                DeepSeekError::Parse(m) => LLMError::Parse(m),
            })?;

        to_llm_response(resp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm_provider::LLMMessage;

    #[test]
    fn llm_message_to_deepseek_message_preserves_role_and_content() {
        let llm_messages = vec![
            LLMMessage::system("you are helpful"),
            LLMMessage::user("hello"),
            LLMMessage::assistant("hi there"),
        ];
        let ds_messages: Vec<DeepSeekMessage> = llm_messages
            .iter()
            .map(|m| DeepSeekMessage {
                role: m.role,
                content: m.content.clone(),
            })
            .collect();

        assert_eq!(ds_messages.len(), 3);
        assert_eq!(ds_messages[0].role, "system");
        assert_eq!(ds_messages[0].content, "you are helpful");
        assert_eq!(ds_messages[1].role, "user");
        assert_eq!(ds_messages[1].content, "hello");
        assert_eq!(ds_messages[2].role, "assistant");
        assert_eq!(ds_messages[2].content, "hi there");
    }

    /// Parse a wire-shaped body and run it through the real conversion, so
    /// these tests exercise `to_llm_response` itself rather than a copy of its
    /// logic that could drift away from it.
    fn convert(json: &str) -> Result<LLMResponse, LLMError> {
        to_llm_response(serde_json::from_str::<DeepSeekResponse>(json).unwrap())
    }

    #[test]
    fn llm_response_extracts_first_choice_and_total_tokens() {
        let resp = convert(
            r#"{
            "choices": [
                {"message": {"role": "assistant", "content": "the reply"}, "finish_reason": "stop"},
                {"message": {"role": "assistant", "content": "second choice ignored"}, "finish_reason": "stop"}
            ],
            "usage": {"total_tokens": 42, "prompt_tokens": 10, "completion_tokens": 32}
        }"#,
        )
        .expect("a normal completion should convert");

        assert_eq!(resp.content, "the reply");
        assert_eq!(resp.total_tokens, 42);
        assert_eq!(resp.finish_reason, "stop");
    }

    #[test]
    fn llm_response_handles_empty_choices_gracefully() {
        let resp = convert(r#"{"choices": [], "usage": {"total_tokens": 5}}"#)
            .expect("no choices is not a truncation");

        assert_eq!(resp.content, "");
        assert_eq!(resp.total_tokens, 5);
        assert_eq!(resp.finish_reason, "");
    }

    #[test]
    fn missing_finish_reason_is_not_treated_as_truncation() {
        // The field is optional on the wire; its absence must not fail a
        // response that is otherwise perfectly usable.
        let resp = convert(
            r#"{
            "choices": [{"message": {"role": "assistant", "content": "fine"}}],
            "usage": {"total_tokens": 7}
        }"#,
        )
        .expect("a response without finish_reason should still convert");

        assert_eq!(resp.content, "fine");
        assert_eq!(resp.finish_reason, "");
    }

    // -- truncation ----------------------------------------------------------
    //
    // The bug these guard against: reasoning tokens are billed against
    // max_tokens without appearing in the output, so a call that runs out of
    // budget comes back looking ordinary — empty or cut off mid-sentence —
    // and only finish_reason says otherwise.

    #[test]
    fn finish_reason_length_is_rejected_as_truncated() {
        let err = convert(
            r#"{
            "choices": [{"message": {"role": "assistant", "content": ""}, "finish_reason": "length"}],
            "usage": {"total_tokens": 4096}
        }"#,
        )
        .expect_err("a truncated completion must not convert to a success");

        assert!(matches!(err, LLMError::Truncated { ref finish_reason } if finish_reason == "length"));
    }

    #[test]
    fn truncation_is_rejected_even_when_partial_content_came_back() {
        // The dangerous case: content is non-empty, so nothing downstream
        // looks wrong until a parser trips over the half-written JSON and
        // blames the model's formatting.
        let err = convert(
            r#"{
            "choices": [{"message": {"role": "assistant", "content": "[{\"question\": \"Wh"}, "finish_reason": "length"}],
            "usage": {"total_tokens": 4096}
        }"#,
        )
        .expect_err("partial content plus finish_reason=length is still truncation");

        assert!(matches!(err, LLMError::Truncated { .. }));
    }

    #[test]
    fn truncation_is_rejected_even_when_the_partial_content_is_valid_json() {
        // The quietest shape of all, and the one no parser can catch: the
        // model closed its brackets and *then* ran out. Here it returned one
        // card where the caller may have asked for several — syntactically
        // perfect, semantically short. Prioritising "it parsed, so use it"
        // would silently deliver part of an answer as the whole of it, with
        // nothing anywhere reporting a problem.
        //
        // So the budget outranks the syntax: a truncated reply is discarded,
        // never salvaged. Only `finish_reason` can tell these apart, which is
        // exactly why it is checked before the content is ever looked at.
        let err = convert(
            r#"{
            "choices": [{"message": {"role": "assistant", "content": "[{\"question\": \"Q1\", \"answer\": \"A1\"}]"}, "finish_reason": "length"}],
            "usage": {"total_tokens": 4096}
        }"#,
        )
        .expect_err("valid JSON plus finish_reason=length is still truncation");

        assert!(matches!(err, LLMError::Truncated { .. }));
    }

    #[test]
    fn truncation_error_message_names_the_cause() {
        // Whoever reads this in a log should not have to guess why an
        // otherwise-successful call produced nothing.
        let err = LLMError::Truncated { finish_reason: "length".to_string() };
        let rendered = err.to_string();
        assert!(rendered.contains("truncated"), "unexpected message: {rendered}");
        assert!(rendered.contains("length"), "unexpected message: {rendered}");
    }

    #[test]
    fn deepseek_error_maps_to_llm_error_variants() {
        let ds_net = DeepSeekError::Network("timeout".into());
        let llm_net = match ds_net {
            DeepSeekError::Network(m) => LLMError::Network(m),
            DeepSeekError::Http { status, body } => LLMError::Http { status, body },
            DeepSeekError::Parse(m) => LLMError::Parse(m),
        };
        assert!(matches!(llm_net, LLMError::Network(s) if s == "timeout"));

        let ds_http = DeepSeekError::Http { status: 429, body: "rate limited".into() };
        let llm_http = match ds_http {
            DeepSeekError::Network(m) => LLMError::Network(m),
            DeepSeekError::Http { status, body } => LLMError::Http { status, body },
            DeepSeekError::Parse(m) => LLMError::Parse(m),
        };
        assert!(matches!(llm_http, LLMError::Http { status: 429, .. }));

        let ds_parse = DeepSeekError::Parse("bad json".into());
        let llm_parse = match ds_parse {
            DeepSeekError::Network(m) => LLMError::Network(m),
            DeepSeekError::Http { status, body } => LLMError::Http { status, body },
            DeepSeekError::Parse(m) => LLMError::Parse(m),
        };
        assert!(matches!(llm_parse, LLMError::Parse(s) if s == "bad json"));
    }

    // -- live check ----------------------------------------------------------
    //
    // #[ignore] by default so the normal `cargo test` run stays offline, same
    // convention as ks_client's live tests. Run it deliberately:
    //
    //     cargo test -p backend -- --ignored --nocapture live_finish_reason
    //
    // A fixture can only prove we handle the value we invented for it. This
    // proves the field is actually present on a real DeepSeek response and
    // reaches LLMResponse — the part no offline test can establish.

    #[tokio::test]
    #[ignore = "requires a real DEEPSEEK_API_KEY and network access"]
    async fn live_finish_reason_is_read_from_a_real_response() {
        dotenvy::dotenv().ok();
        dotenvy::from_filename("../.env").ok();

        let client = DeepSeekClient::from_env()
            .expect("DEEPSEEK_API_KEY must be set in .env to run this test");

        let messages = vec![LLMMessage::user("Reply with the single word: ok")];
        let resp = LLMProvider::chat_completion(&client, &messages, None)
            .await
            .expect("a short prompt should complete without truncation");

        eprintln!(
            "[live] finish_reason={:?} total_tokens={} content={:?}",
            resp.finish_reason, resp.total_tokens, resp.content
        );

        // The value must come from the wire, not from our Default. If this is
        // empty, the field is not being parsed and every truncation would slip
        // through undetected.
        assert!(
            !resp.finish_reason.is_empty(),
            "finish_reason came back empty — the field is not being read off the real response"
        );
        assert_eq!(
            resp.finish_reason, "stop",
            "a short prompt should stop normally"
        );
    }
}