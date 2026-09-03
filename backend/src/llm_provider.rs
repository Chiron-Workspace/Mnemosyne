use async_trait::async_trait;

pub struct LLMMessage {
    pub role: &'static str,
    pub content: String,
}

impl LLMMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self { role: "system", content: content.into() }
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self { role: "user", content: content.into() }
    }
    pub fn assistant(content: impl Into<String>) -> Self {
        Self { role: "assistant", content: content.into() }
    }
}

#[derive(Debug)]
pub struct LLMResponse {
    pub content: String,
    pub total_tokens: u32,
    /// Why the model stopped generating, verbatim from the provider
    /// (`"stop"` on a normal completion). A response that ran out of token
    /// budget never reaches here — providers must reject it as
    /// [`LLMError::Truncated`] rather than hand back partial content — so this
    /// is kept for logging and for spotting future stop reasons we do not yet
    /// handle, not as something callers are expected to branch on — which is
    /// why nothing reads it today.
    #[allow(dead_code)]
    pub finish_reason: String,
}

#[derive(Debug)]
pub enum LLMError {
    Network(String),
    Http { status: u16, body: String },
    Parse(String),
    /// The model hit its token budget mid-answer (`finish_reason == "length"`).
    ///
    /// Carries `total_tokens` because this is the one failure that provably
    /// cost something: the provider reports usage for a truncated completion
    /// exactly as it does for a finished one, and the whole budget was spent —
    /// mostly on reasoning that never reached the output. A failed call that
    /// records zero tokens would make the most expensive kind of failure the
    /// one that looks free.
    ///
    /// This is its own variant because the failure is invisible in the payload:
    /// reasoning tokens count against the budget without appearing in the
    /// output, so a truncated call can come back with `content` empty or cut
    /// off mid-sentence and otherwise look like a success. Feeding that to a
    /// parser produces a confusing "bad JSON" error that blames the model's
    /// formatting for what is really a budget problem. It is transient —
    /// retrying, or asking for less, can succeed.
    Truncated { finish_reason: String, total_tokens: u32 },
}

impl std::fmt::Display for LLMError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LLMError::Network(m) => write!(f, "network error: {m}"),
            LLMError::Http { status, body } => {
                let snippet: String = body.chars().take(500).collect();
                write!(f, "upstream HTTP {status}: {snippet}")
            }
            LLMError::Parse(m) => write!(f, "parse error: {m}"),
            LLMError::Truncated { finish_reason, total_tokens } => write!(
                f,
                "response truncated by the token budget (finish_reason: {finish_reason}, \
                 {total_tokens} tokens spent); no usable content was returned"
            ),
        }
    }
}

impl std::error::Error for LLMError {}

#[async_trait]
pub trait LLMProvider: Send + Sync {
    async fn chat_completion(
        &self,
        messages: &[LLMMessage],
        model: Option<&str>,
    ) -> Result<LLMResponse, LLMError>;
}
