//! HTTP handlers grouped by resource.
//!
//! Each submodule corresponds to one schema table. The shared [`error`]
//! helper enforces a single, consistent JSON error shape across all
//! endpoints in this module: `{"error": "<message>"}`.

pub mod cards;
pub mod cards_from_node;
pub mod due;
pub mod feynman;
pub mod generate;
pub mod quiz;
pub mod reviews;
pub mod socratic;
pub mod study_sets;
pub mod users;

#[cfg(test)]
pub mod test_db;

use actix_web::HttpResponse;

use crate::llm_provider::LLMError;

/// Consistent error JSON shape for all CRUD endpoints in this module.
pub fn error_response(status: actix_web::http::StatusCode, message: impl Into<String>) -> HttpResponse {
    HttpResponse::build(status).json(serde_json::json!({ "error": message.into() }))
}

/// Everything an AI handler needs to record and report a failed provider call.
pub struct LlmFailure {
    /// What to write to `ai_interactions.output_text`.
    pub placeholder: String,
    /// What to tell the caller.
    pub message: String,
    /// What the call actually cost. See [`describe_llm_failure`] for why this
    /// is zero for every failure except truncation.
    pub tokens_used: u32,
}

/// Render an [`LLMError`] into what every AI handler needs when a provider
/// call fails.
///
/// Shared rather than repeated per handler so that truncation stays
/// distinguishable everywhere. The generic phrasing ("no response received")
/// is a lie for a truncated call — the model did answer, it just ran out of
/// budget partway — and that difference decides what to do next: a truncated
/// call is worth retrying or asking for less, whereas a 401 never is. Both
/// still map to 502 for the client, since either way this service could not
/// produce the answer it promised.
///
/// # What `tokens_used` means, and does not
///
/// Only truncation reports a real figure. The provider returns `usage` for a
/// truncated completion exactly as it does for a finished one, and that number
/// matters: measurement has shown a truncated call spending its entire budget,
/// almost all of it on reasoning that never reached the output. Recording zero
/// there would make the most expensive kind of failure the one that looks free.
///
/// For the rest, zero is the honest figure for a different reason in each case,
/// and in one of them it is not really a figure at all:
///
/// - [`LLMError::Network`] — nothing arrived, and a request that never reached
///   the provider is not billed. Zero is true.
/// - [`LLMError::Http`] — a rejected request (401, 429) is not billed, and the
///   provider's error bodies carry no `usage` regardless. Zero is true.
/// - [`LLMError::Parse`] — **zero here means "unknown", not "free".** The body
///   did not match the schema, so whatever `usage` it may have held could not
///   be read; a 2xx with an empty body (which has happened — see
///   `docs/gotchas.md`) may well have been billed. Distinguishing the two would
///   need a nullable column, which is a schema change and not this function's
///   to make. Until then, the accompanying `placeholder` says plainly that the
///   call failed, so nobody reading the row mistakes it for a free success.
pub fn describe_llm_failure(err: &LLMError) -> LlmFailure {
    match err {
        LLMError::Truncated { finish_reason, total_tokens } => LlmFailure {
            placeholder: format!("[truncated by the token budget (finish_reason: {finish_reason}) — no usable content returned]"),
            message: format!("DeepSeek stopped mid-answer at its token limit ({finish_reason}); nothing was parsed. Retrying, or requesting fewer items, may succeed."),
            tokens_used: *total_tokens,
        },
        other => LlmFailure {
            placeholder: format!("[no response received from DeepSeek — call failed: {other}]"),
            message: format!("DeepSeek API call failed: {other}"),
            tokens_used: 0,
        },
    }
}

/// Convenience: extract a Postgres SQLSTATE code from a sqlx error as an
/// owned `String` (sqlx returns `Cow<str>`, which we can't borrow across the
/// helper boundary). Used to map DB-level violations (unique, FK) to
/// appropriate HTTP statuses instead of an unhandled 500.
fn pg_sqlstate(err: &sqlx::Error) -> Option<String> {
    err.as_database_error().and_then(|e| e.code()).map(|c| c.into_owned())
}

#[cfg(test)]
mod tests {
    use super::{describe_llm_failure, LlmFailure};
    use crate::llm_provider::LLMError;

    #[test]
    fn truncation_is_reported_as_truncation_not_as_a_dead_call() {
        let LlmFailure { placeholder, message, .. } =
            describe_llm_failure(&LLMError::Truncated { finish_reason: "length".into(), total_tokens: 4096 });

        // What lands in ai_interactions.output_text must say the model did
        // answer and was cut off, not that nothing came back — the two call
        // for different responses from whoever reads the log.
        assert!(placeholder.contains("truncated"), "placeholder: {placeholder}");
        assert!(!placeholder.contains("no response received"), "placeholder: {placeholder}");
        assert!(message.contains("token limit"), "message: {message}");
    }

    #[test]
    fn other_failures_keep_the_original_phrasing() {
        let LlmFailure { placeholder, message, .. } =
            describe_llm_failure(&LLMError::Http { status: 401, body: "unauthorized".into() });

        assert!(placeholder.contains("no response received"), "placeholder: {placeholder}");
        assert!(message.contains("401"), "message: {message}");
    }

    #[test]
    fn every_failure_kind_produces_a_non_empty_pair() {
        for err in [
            LLMError::Network("timeout".into()),
            LLMError::Http { status: 500, body: "boom".into() },
            LLMError::Parse("bad json".into()),
            LLMError::Truncated { finish_reason: "length".into(), total_tokens: 4096 },
        ] {
            let failure = describe_llm_failure(&err);
            assert!(!failure.placeholder.is_empty(), "empty placeholder for {err:?}");
            assert!(!failure.message.is_empty(), "empty message for {err:?}");
        }
    }

    #[test]
    fn a_truncated_call_reports_what_it_spent() {
        // The whole point: this failure is billed in full, mostly for
        // reasoning nobody ever sees. Logging zero would hide the most
        // expensive failure there is behind the cheapest-looking number.
        let failure = describe_llm_failure(&LLMError::Truncated {
            finish_reason: "length".into(),
            total_tokens: 4096,
        });
        assert_eq!(failure.tokens_used, 4096);
    }

    #[test]
    fn failures_that_report_no_usage_record_zero() {
        // Zero for a reason that differs per variant — nothing was billed for
        // Network and Http; for Parse the number simply did not survive the
        // body. See this function's docs.
        for err in [
            LLMError::Network("timeout".into()),
            LLMError::Http { status: 401, body: "unauthorized".into() },
            LLMError::Parse("EOF while parsing a value".into()),
        ] {
            assert_eq!(describe_llm_failure(&err).tokens_used, 0, "for {err:?}");
        }
    }
}

/// Classify a sqlx error into an HTTP status suitable for the CRUD layer:
/// - 23505 (unique_violation) -> 409 Conflict
/// - 23503 (foreign_key_violation) -> 400 Bad Request
/// - everything else -> 500 Internal Server Error
pub fn classify_db_error(err: &sqlx::Error) -> (actix_web::http::StatusCode, String) {
    match pg_sqlstate(err).as_deref() {
        Some("23505") => (
            actix_web::http::StatusCode::CONFLICT,
            "resource already exists (unique constraint violation)".to_string(),
        ),
        Some("23503") => (
            actix_web::http::StatusCode::BAD_REQUEST,
            "referenced resource does not exist (foreign key violation)".to_string(),
        ),
        _ => (
            actix_web::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("database error: {err}"),
        ),
    }
}