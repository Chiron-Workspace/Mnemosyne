//! HTTP handlers grouped by resource.
//!
//! Each submodule corresponds to one schema table. The shared [`error`]
//! helper enforces a single, consistent JSON error shape across all
//! endpoints in this module: `{"error": "<message>"}`.

pub mod cards;
pub mod due;
pub mod feynman;
pub mod generate;
pub mod quiz;
pub mod reviews;
pub mod socratic;
pub mod study_sets;
pub mod users;

use actix_web::HttpResponse;

/// Consistent error JSON shape for all CRUD endpoints in this module.
pub fn error_response(status: actix_web::http::StatusCode, message: impl Into<String>) -> HttpResponse {
    HttpResponse::build(status).json(serde_json::json!({ "error": message.into() }))
}

/// Convenience: extract a Postgres SQLSTATE code from a sqlx error as an
/// owned `String` (sqlx returns `Cow<str>`, which we can't borrow across the
/// helper boundary). Used to map DB-level violations (unique, FK) to
/// appropriate HTTP statuses instead of an unhandled 500.
fn pg_sqlstate(err: &sqlx::Error) -> Option<String> {
    err.as_database_error().and_then(|e| e.code()).map(|c| c.into_owned())
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