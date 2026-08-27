//! Handler for `POST /review` — the first endpoint wiring `mnemosyne-core`'s
//! FSRS scheduler into the database. Records a review attempt for a card by
//! a user, computes the updated FSRS scheduling state, and persists both the
//! event and the new state to `learning_events`.

use actix_web::{post, web, HttpResponse};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

use mnemosyne_core::models::{CardState, Rating};
use mnemosyne_core::scheduling::FsrsScheduler;

use super::{classify_db_error, error_response};

/// Incoming review request. `rating` is parsed case-insensitively to
/// [`mnemosyne_core::models::Rating`]; anything else is rejected with 400.
#[derive(Debug, Deserialize)]
pub struct ReviewRequest {
    pub card_id: Uuid,
    pub user_id: Uuid,
    pub rating: String,
}

/// Response returned on a successful review (HTTP 201).
#[derive(Debug, Serialize)]
pub struct ReviewResponse {
    pub learning_event_id: Uuid,
    pub stability: f32,
    pub difficulty: f32,
    pub interval_days: i64,
    pub next_review_at: DateTime<Utc>,
}

/// Row used to reconstruct the prior FSRS scheduling state for a (card, user)
/// pair. `stability`/`difficulty` are nullable in the schema (legacy pre-FSRS
/// rows would have NULL); we use `Option<f64>` because Postgres `FLOAT` (no
/// precision specifier) is `FLOAT8` (double precision), and sqlx maps `FLOAT8`
/// to `f64`.
#[derive(Debug, FromRow)]
struct PriorEventRow {
    stability: Option<f64>,
    difficulty: Option<f64>,
    created_at: DateTime<Utc>,
}

/// Row returned by `INSERT ... RETURNING` for the new learning_event.
#[derive(Debug, FromRow)]
struct InsertedEventRow {
    id: Uuid,
    stability: Option<f64>,
    difficulty: Option<f64>,
    interval: i32,
    next_review_at: DateTime<Utc>,
}

/// Parse the incoming rating string (case-insensitive) to the FSRS `Rating`
/// enum. Returns `Err(())` on anything not in {again, hard, good, easy}.
fn parse_rating(s: &str) -> Result<Rating, ()> {
    match s.to_lowercase().as_str() {
        "again" => Ok(Rating::Again),
        "hard" => Ok(Rating::Hard),
        "good" => Ok(Rating::Good),
        "easy" => Ok(Rating::Easy),
        _ => Err(()),
    }
}

#[post("/review")]
pub async fn review(
    pool: web::Data<PgPool>,
    scheduler: web::Data<FsrsScheduler>,
    body: web::Json<ReviewRequest>,
) -> HttpResponse {
    // 1. Parse rating (case-insensitive). Reject bad strings with 400 — do
    //    not silently default.
    let rating = match parse_rating(&body.rating) {
        Ok(r) => r,
        Err(_) => {
            return error_response(
                actix_web::http::StatusCode::BAD_REQUEST,
                format!(
                    "invalid rating '{}': must be one of again, hard, good, easy",
                    body.rating
                ),
            );
        }
    };

    let now = Utc::now();
    // Deliberate product decision: Again = forgotten, everything else = correct
    // (matches Anki/FSRS convention).
    let is_correct = rating != Rating::Again;

    // 2. Look up prior scheduling state for this (card, user) pair. If no
    //    prior event exists (or it predates FSRS adoption and has NULL
    //    stability/difficulty), treat as a brand-new card.
    let prior: Option<PriorEventRow> = match sqlx::query_as::<_, PriorEventRow>(
        r#"SELECT stability, difficulty, created_at
           FROM learning_events
           WHERE card_id = $1 AND user_id = $2
           ORDER BY created_at DESC
           LIMIT 1"#,
    )
    .bind(body.card_id)
    .bind(body.user_id)
    .fetch_optional(pool.get_ref())
    .await
    {
        Ok(row) => row,
        Err(e) => {
            return error_response(
                actix_web::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("database error looking up prior state: {e}"),
            );
        }
    };

    // 3. Compute the updated CardState via the FSRS scheduler.
    let resulting_state: CardState = match prior {
        Some(p) if p.stability.is_some() && p.difficulty.is_some() => {
            // Follow-up review: reconstruct prior state, call schedule_review.
            // `last_review` is when the prior event happened = its created_at.
            // `due` is informational here — schedule_review ignores it (it
            // computes elapsed from last_review).
            let current = CardState {
                stability: p.stability.unwrap() as f32,
                difficulty: p.difficulty.unwrap() as f32,
                due: now,
                last_review: Some(p.created_at),
            };
            match scheduler.schedule_review(&current, rating, now) {
                Ok((state, _log)) => state,
                Err(e) => {
                    return error_response(
                        actix_web::http::StatusCode::INTERNAL_SERVER_ERROR,
                        format!("scheduler error: {e}"),
                    );
                }
            }
        }
        _ => {
            // First review (or prior was pre-FSRS): use the schedule_new path,
            // pick the state matching the learner's actual rating.
            let states = match scheduler.schedule_new(now) {
                Ok(s) => s,
                Err(e) => {
                    return error_response(
                        actix_web::http::StatusCode::INTERNAL_SERVER_ERROR,
                        format!("scheduler error: {e}"),
                    );
                }
            };
            match rating {
                Rating::Again => states.again,
                Rating::Hard => states.hard,
                Rating::Good => states.good,
                Rating::Easy => states.easy,
            }
        }
    };

    // 4. Derive interval (days) and next_review_at from the resulting state.
    //    The scheduler sets `due = now + round(interval).max(1) days`, so we
    //    can recover the integer interval by subtracting now from due.
    let interval_days = (resulting_state.due - now).num_days().max(1);

    // 5. Insert a new learning_events row. card_id/user_id existence is
    //    enforced by FK constraints; a violation is mapped to 400 by
    //    classify_db_error(). We leave `ease_factor` at its schema default
    //    (2.5) — it's legacy/unused post-FSRS (see ADR 0001).
    let inserted: InsertedEventRow = match sqlx::query_as::<_, InsertedEventRow>(
        r#"INSERT INTO learning_events
             (card_id, user_id, is_correct, stability, difficulty, interval, next_review_at)
           VALUES ($1, $2, $3, $4, $5, $6, $7)
           RETURNING id, stability, difficulty, interval, next_review_at"#,
    )
    .bind(body.card_id)
    .bind(body.user_id)
    .bind(is_correct)
    .bind(resulting_state.stability as f64)
    .bind(resulting_state.difficulty as f64)
    .bind(interval_days as i32)
    .bind(resulting_state.due)
    .fetch_one(pool.get_ref())
    .await
    {
        Ok(row) => row,
        Err(e) => {
            let (status, msg) = classify_db_error(&e);
            return error_response(status, msg);
        }
    };

    // 6. Respond with the persisted state (read back from the DB so the
    //    response reflects exactly what was stored, not the in-memory value).
    HttpResponse::Created().json(ReviewResponse {
        learning_event_id: inserted.id,
        stability: inserted.stability.unwrap_or(0.0) as f32,
        difficulty: inserted.difficulty.unwrap_or(0.0) as f32,
        interval_days: inserted.interval as i64,
        next_review_at: inserted.next_review_at,
    })
}