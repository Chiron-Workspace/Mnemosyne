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

pub struct LLMResponse {
    pub content: String,
    pub total_tokens: u32,
}

#[derive(Debug)]
pub enum LLMError {
    Network(String),
    Http { status: u16, body: String },
    Parse(String),
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
