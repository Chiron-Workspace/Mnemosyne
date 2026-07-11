//! `GET /due` — read-only review queue for a user.
//!
//! Returns cards the user should review now: either never-reviewed cards, or
//! cards whose most recent `learning_events` row has `next_review_at <= now()`.
//!
//! Ordering: never-reviewed cards (NULL `next_review_at`) surface first, then
//! cards by `next_review_at ASC` (most overdue first). This is a deliberate
//! product decision — see the prompt spec: don't change it.

use actix_web::{get, web, HttpResponse};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

use super::error_response;

/// Cap on `limit` to bound query cost.
const DEFAULT_LIMIT: i64 = 20;
const MAX_LIMIT: i64 = 100;

#[derive(Debug, Deserialize)]
pub struct DueQuery {
    pub user_id: Option<Uuid>,
    pub limit: Option<i64>,
}

/// One row of the result. `stability`, `difficulty`, `next_review_at` are
/// nullable because never-reviewed cards have no FSRS state yet.
#[derive(Debug, Serialize, FromRow)]
struct DueCardRow {
    card_id: Uuid,
    set_id: Uuid,
    question: String,
    answer: String,
    stability: Option<f64>,
    difficulty: Option<f64>,
    next_review_at: Option<DateTime<Utc>>,
}

/// Response shape returned on 200.
#[derive(Debug, Serialize)]
struct DueResponse {
    due_cards: Vec<DueCardOut>,
    count: i64,
}

/// Per-card output with `is_new` derived from whether `next_review_at` is NULL.
#[derive(Debug, Serialize)]
struct DueCardOut {
    card_id: Uuid,
    set_id: Uuid,
    question: String,
    answer: String,
    is_new: bool,
    stability: Option<f64>,
    difficulty: Option<f64>,
    next_review_at: Option<DateTime<Utc>>,
}

impl From<DueCardRow> for DueCardOut {
    fn from(r: DueCardRow) -> Self {
        let is_new = r.next_review_at.is_none();
        DueCardOut {
            card_id: r.card_id,
            set_id: r.set_id,
            question: r.question,
            answer: r.answer,
            is_new,
            stability: r.stability,
            difficulty: r.difficulty,
            next_review_at: r.next_review_at,
        }
    }
}

#[get("/due")]
pub async fn due(pool: web::Data<PgPool>, query: web::Query<DueQuery>) -> HttpResponse {
    // 1. user_id is required. Well-formed but nonexistent user returns an
    //    empty array (not an error) — the spec says a user with zero cards
    //    isn't a bug, so the same SQL naturally returns [].
    let Some(user_id) = query.user_id else {
        return error_response(
            actix_web::http::StatusCode::BAD_REQUEST,
            "user_id query parameter is required",
        );
    };

    // 2. limit: default 20, clamp to [1, 100]. A limit of 0 is silly;
    //    silently raise it to 1 rather than 400 — saves the caller a round
    //    trip for no benefit, and the spec doesn't mention a 0 case.
    let limit = query.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);

    // 3. Run the due-card query. LATERAL join fetches the latest
    //    learning_events row per (card, user) pair in a single round trip;
    //    the LEFT JOIN lets cards with no events through (NULL columns),
    //    which is how we mark them as `is_new`.
    //
    //    `.persistent(false)` per docs/gotchas.md — Supabase pooler doesn't
    //    support cached prepared statements.
    let rows: Vec<DueCardRow> = match sqlx::query_as::<_, DueCardRow>(
        r#"SELECT c.id AS card_id, c.set_id, c.question, c.answer,
                  le.stability, le.difficulty, le.next_review_at
           FROM cards c
           JOIN study_sets ss ON ss.id = c.set_id
           LEFT JOIN LATERAL (
               SELECT stability, difficulty, next_review_at
               FROM learning_events
               WHERE card_id = c.id AND user_id = $1
               ORDER BY created_at DESC
               LIMIT 1
           ) le ON true
           WHERE ss.user_id = $1
             AND (le.next_review_at IS NULL OR le.next_review_at <= now())
           ORDER BY le.next_review_at ASC NULLS FIRST
           LIMIT $2"#,
    )
    .persistent(false)
    .bind(user_id)
    .bind(limit)
    .fetch_all(pool.get_ref())
    .await
    {
        Ok(r) => r,
        Err(e) => {
            return error_response(
                actix_web::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("database error: {e}"),
            );
        }
    };

    let due_cards: Vec<DueCardOut> = rows.into_iter().map(DueCardOut::from).collect();
    let count = due_cards.len() as i64;

    HttpResponse::Ok().json(DueResponse { due_cards, count })
}