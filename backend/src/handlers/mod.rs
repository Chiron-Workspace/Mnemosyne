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

use actix_web::HttpResponse;

use crate::llm_provider::LLMError;

/// Consistent error JSON shape for all CRUD endpoints in this module.
pub fn error_response(status: actix_web::http::StatusCode, message: impl Into<String>) -> HttpResponse {
    HttpResponse::build(status).json(serde_json::json!({ "error": message.into() }))
}

/// Render an [`LLMError`] into the two strings every AI handler needs when a
/// provider call fails: what to record as `ai_interactions.output_text`, and
/// what to tell the caller.
///
/// Shared rather than repeated per handler so that truncation stays
/// distinguishable everywhere. The generic phrasing ("no response received")
/// is a lie for a truncated call — the model did answer, it just ran out of
/// budget partway — and that difference decides what to do next: a truncated
/// call is worth retrying or asking for less, whereas a 401 never is. Both
/// still map to 502 for the client, since either way this service could not
/// produce the answer it promised.
pub fn describe_llm_failure(err: &LLMError) -> (String, String) {
    match err {
        LLMError::Truncated { finish_reason } => (
            format!("[truncated by the token budget (finish_reason: {finish_reason}) — no usable content returned]"),
            format!("DeepSeek stopped mid-answer at its token limit ({finish_reason}); nothing was parsed. Retrying, or requesting fewer items, may succeed."),
        ),
        other => (
            format!("[no response received from DeepSeek — call failed: {other}]"),
            format!("DeepSeek API call failed: {other}"),
        ),
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
    use super::describe_llm_failure;
    use crate::llm_provider::LLMError;

    #[test]
    fn truncation_is_reported_as_truncation_not_as_a_dead_call() {
        let (placeholder, message) =
            describe_llm_failure(&LLMError::Truncated { finish_reason: "length".into() });

        // What lands in ai_interactions.output_text must say the model did
        // answer and was cut off, not that nothing came back — the two call
        // for different responses from whoever reads the log.
        assert!(placeholder.contains("truncated"), "placeholder: {placeholder}");
        assert!(!placeholder.contains("no response received"), "placeholder: {placeholder}");
        assert!(message.contains("token limit"), "message: {message}");
    }

    #[test]
    fn other_failures_keep_the_original_phrasing() {
        let (placeholder, message) =
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
            LLMError::Truncated { finish_reason: "length".into() },
        ] {
            let (placeholder, message) = describe_llm_failure(&err);
            assert!(!placeholder.is_empty(), "empty placeholder for {err:?}");
            assert!(!message.is_empty(), "empty message for {err:?}");
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