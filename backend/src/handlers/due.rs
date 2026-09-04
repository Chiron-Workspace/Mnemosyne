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

/// The due-card query. Named so tests run the same text the handler does.
///
/// Three separate decisions live in here, and each of them is silent when
/// wrong — a broken one returns a plausible list rather than an error:
///
/// - the LATERAL picks the **latest** event per card, not the first;
/// - `next_review_at <= now()` withholds cards that are scheduled but not yet
///   due, while `IS NULL` lets never-reviewed cards through;
/// - `ss.user_id = $1` scopes to the study set's owner, so one learner's queue
///   cannot show another's cards.
const DUE_QUERY: &str = r#"SELECT c.id AS card_id, c.set_id, c.question, c.answer,
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
   LIMIT $2"#;

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

/// Default 20, clamped to [1, 100]. A limit of 0 is silly; it is silently
/// raised to 1 rather than rejected — that saves the caller a round trip and
/// costs nothing, since no caller wants an empty page on purpose.
fn effective_limit(requested: Option<i64>) -> i64 {
    requested.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT)
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
    let limit = effective_limit(query.limit);

    // 3. Run the due-card query. LATERAL join fetches the latest
    //    learning_events row per (card, user) pair in a single round trip;
    //    the LEFT JOIN lets cards with no events through (NULL columns),
    //    which is how we mark them as `is_new`.
    let rows: Vec<DueCardRow> = match sqlx::query_as::<_, DueCardRow>(DUE_QUERY)
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
#[cfg(test)]
mod tests {
    use super::*;
    use crate::handlers::test_db;
    use chrono::Duration;

    // -- pure -----------------------------------------------------------------

    #[test]
    fn limit_defaults_and_clamps() {
        assert_eq!(effective_limit(None), DEFAULT_LIMIT);
        assert_eq!(effective_limit(Some(50)), 50);
        assert_eq!(effective_limit(Some(MAX_LIMIT)), MAX_LIMIT);
        // Above the cap: bounded rather than rejected, so one careless caller
        // cannot ask for the whole table.
        assert_eq!(effective_limit(Some(10_000)), MAX_LIMIT);
        // Zero and negatives mean nobody wants an empty page; give them one card.
        assert_eq!(effective_limit(Some(0)), 1);
        assert_eq!(effective_limit(Some(-5)), 1);
    }

    fn row(next_review_at: Option<DateTime<Utc>>) -> DueCardRow {
        DueCardRow {
            card_id: Uuid::nil(),
            set_id: Uuid::nil(),
            question: "q".into(),
            answer: "a".into(),
            stability: next_review_at.map(|_| 1.5),
            difficulty: next_review_at.map(|_| 5.0),
            next_review_at,
        }
    }

    #[test]
    fn is_new_is_derived_from_the_absence_of_a_schedule() {
        // A card is "new" to the learner exactly when no review has ever set a
        // next date for it. The flag is not stored anywhere; it is this.
        assert!(DueCardOut::from(row(None)).is_new);
        assert!(!DueCardOut::from(row(Some(Utc::now()))).is_new);
    }

    // -- against the real database (see handlers::test_db) ---------------------

    #[tokio::test]
    #[ignore = "requires the local Postgres cluster"]
    async fn due_puts_never_reviewed_cards_before_scheduled_ones() {
        let pool = test_db::pool().await;
        let mut tx = pool.begin().await.unwrap();
        let (user_id, set_id) = test_db::seed_learner(&mut tx).await;

        let seen = test_db::seed_card(&mut tx, set_id, "already reviewed").await;
        let fresh = test_db::seed_card(&mut tx, set_id, "never reviewed").await;
        let now = Utc::now();
        test_db::record_review(&mut tx, seen, user_id, 1.0, 5.0, now - Duration::days(1), now)
            .await;

        let rows: Vec<DueCardRow> = sqlx::query_as(DUE_QUERY)
            .bind(user_id)
            .bind(20_i64)
            .fetch_all(&mut *tx)
            .await
            .unwrap();

        // NULLS FIRST: a card the learner has never seen outranks one that is
        // merely overdue. Deliberate product decision, documented at the top of
        // this module — this test is what keeps it from being "optimised" away.
        let order: Vec<Uuid> = rows.iter().map(|r| r.card_id).collect();
        assert_eq!(order, vec![fresh, seen], "new card should come first");
    }

    #[tokio::test]
    #[ignore = "requires the local Postgres cluster"]
    async fn due_reads_the_latest_review_of_a_card_not_the_first() {
        let pool = test_db::pool().await;
        let mut tx = pool.begin().await.unwrap();
        let (user_id, set_id) = test_db::seed_learner(&mut tx).await;
        let card = test_db::seed_card(&mut tx, set_id, "reviewed twice").await;

        let now = Utc::now();
        // Values chosen far apart so a wrong pick cannot look like rounding.
        test_db::record_review(&mut tx, card, user_id, 9.0, 2.0, now - Duration::days(2), now - Duration::days(2)).await;
        test_db::record_review(&mut tx, card, user_id, 0.5, 8.0, now - Duration::days(1), now - Duration::days(1)).await;

        let rows: Vec<DueCardRow> = sqlx::query_as(DUE_QUERY)
            .bind(user_id)
            .bind(20_i64)
            .fetch_all(&mut *tx)
            .await
            .unwrap();

        assert_eq!(rows.len(), 1);
        // FSRS compounds: scheduling from the first review instead of the
        // latest would reset the learner's progress on every card, silently.
        assert_eq!(rows[0].stability, Some(0.5), "took the wrong review");
        assert_eq!(rows[0].difficulty, Some(8.0));
    }

    #[tokio::test]
    #[ignore = "requires the local Postgres cluster"]
    async fn due_withholds_a_card_scheduled_for_the_future() {
        let pool = test_db::pool().await;
        let mut tx = pool.begin().await.unwrap();
        let (user_id, set_id) = test_db::seed_learner(&mut tx).await;
        let card = test_db::seed_card(&mut tx, set_id, "not due yet").await;

        let now = Utc::now();
        test_db::record_review(&mut tx, card, user_id, 4.0, 5.0, now + Duration::days(5), now)
            .await;

        let rows: Vec<DueCardRow> = sqlx::query_as(DUE_QUERY)
            .bind(user_id)
            .bind(20_i64)
            .fetch_all(&mut *tx)
            .await
            .unwrap();

        // Spaced repetition is the whole product: showing a card before its
        // interval has elapsed is not a harmless extra, it undoes the spacing.
        assert!(rows.is_empty(), "a card due in 5 days must not be offered now");
    }

    #[tokio::test]
    #[ignore = "requires the local Postgres cluster"]
    async fn due_never_shows_another_learners_cards() {
        let pool = test_db::pool().await;
        let mut tx = pool.begin().await.unwrap();
        let (mine, my_set) = test_db::seed_learner(&mut tx).await;
        let (_theirs, their_set) = test_db::seed_learner(&mut tx).await;

        let my_card = test_db::seed_card(&mut tx, my_set, "mine").await;
        test_db::seed_card(&mut tx, their_set, "theirs").await;

        let rows: Vec<DueCardRow> = sqlx::query_as(DUE_QUERY)
            .bind(mine)
            .bind(20_i64)
            .fetch_all(&mut *tx)
            .await
            .unwrap();

        // There is no auth layer, so this WHERE clause is the only thing
        // separating two learners' queues.
        let ids: Vec<Uuid> = rows.iter().map(|r| r.card_id).collect();
        assert_eq!(ids, vec![my_card]);
    }
}
