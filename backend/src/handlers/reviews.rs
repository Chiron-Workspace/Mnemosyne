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
use mnemosyne_core::scheduling::{FsrsScheduler, NewCardStates};

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

/// The prior-state lookup. Named so that tests run the same text the handler
/// does; a test holding its own copy would keep passing after this one drifted.
///
/// `ORDER BY created_at DESC LIMIT 1` is the whole point: FSRS compounds, so
/// reading the *first* review instead of the *latest* would quietly reschedule
/// every card from its original state forever.
const PRIOR_STATE_QUERY: &str = r#"SELECT stability, difficulty, created_at
   FROM learning_events
   WHERE card_id = $1 AND user_id = $2
   ORDER BY created_at DESC
   LIMIT 1"#;

/// The event insert. `card_id`/`user_id` are FK columns, and that is the only
/// thing standing between a review and a card that does not exist.
const INSERT_EVENT_QUERY: &str = r#"INSERT INTO learning_events
     (card_id, user_id, is_correct, stability, difficulty, interval, next_review_at)
   VALUES ($1, $2, $3, $4, $5, $6, $7)
   RETURNING id, stability, difficulty, interval, next_review_at"#;

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

/// Deliberate product decision: Again = forgotten, everything else = correct
/// (matches the Anki/FSRS convention). Hard counts as correct — the learner
/// did recall it, with effort.
fn is_correct(rating: Rating) -> bool {
    rating != Rating::Again
}

/// Pick the state matching the learner's rating out of the four the scheduler
/// produces for a brand-new card.
fn state_for_rating(states: NewCardStates, rating: Rating) -> CardState {
    match rating {
        Rating::Again => states.again,
        Rating::Hard => states.hard,
        Rating::Good => states.good,
        Rating::Easy => states.easy,
    }
}

/// Recover the integer day interval the scheduler chose.
///
/// The floor of 1 is not cosmetic: `num_days()` truncates, so any interval the
/// scheduler set to less than a full day would come back as 0 and be written
/// as "due immediately, forever".
fn interval_days_between(due: DateTime<Utc>, now: DateTime<Utc>) -> i64 {
    (due - now).num_days().max(1)
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
    let is_correct = is_correct(rating);

    // 2. Look up prior scheduling state for this (card, user) pair. If no
    //    prior event exists (or it predates FSRS adoption and has NULL
    //    stability/difficulty), treat as a brand-new card.
    let prior: Option<PriorEventRow> = match sqlx::query_as::<_, PriorEventRow>(PRIOR_STATE_QUERY)
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
            state_for_rating(states, rating)
        }
    };

    // 4. Derive interval (days) and next_review_at from the resulting state.
    //    The scheduler sets `due = now + round(interval).max(1) days`, so we
    //    can recover the integer interval by subtracting now from due.
    let interval_days = interval_days_between(resulting_state.due, now);

    // 5. Insert a new learning_events row. card_id/user_id existence is
    //    enforced by FK constraints; a violation is mapped to 400 by
    //    classify_db_error(). We leave `ease_factor` at its schema default
    //    (2.5) — it's legacy/unused post-FSRS (see ADR 0001).
    let inserted: InsertedEventRow = match sqlx::query_as::<_, InsertedEventRow>(INSERT_EVENT_QUERY)
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
#[cfg(test)]
mod tests {
    use super::*;
    use crate::handlers::test_db;
    use chrono::Duration;

    // -- pure -----------------------------------------------------------------

    #[test]
    fn ratings_are_parsed_case_insensitively() {
        assert_eq!(parse_rating("again"), Ok(Rating::Again));
        assert_eq!(parse_rating("Hard"), Ok(Rating::Hard));
        assert_eq!(parse_rating("GOOD"), Ok(Rating::Good));
        assert_eq!(parse_rating("eAsY"), Ok(Rating::Easy));
    }

    #[test]
    fn anything_outside_the_four_ratings_is_rejected() {
        // Rejected rather than defaulted: silently reading an unknown rating as
        // "good" would reschedule a card the learner may have just failed, and
        // nothing downstream would ever show that it happened.
        for bad in ["", "  ", "ok", "3", "forgot", "again ", "trop facile"] {
            assert!(parse_rating(bad).is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn only_again_counts_as_forgetting() {
        // Hard is a success: the learner did recall it, with effort. Treating
        // it as a failure would collapse a four-way rating into pass/fail and
        // throw away the signal FSRS uses most.
        assert!(!is_correct(Rating::Again));
        assert!(is_correct(Rating::Hard));
        assert!(is_correct(Rating::Good));
        assert!(is_correct(Rating::Easy));
    }

    #[test]
    fn each_rating_selects_its_own_scheduled_state() {
        // The four states differ; picking the wrong arm would schedule the card
        // as though the learner had answered something they did not.
        let scheduler = FsrsScheduler::default();
        let now = Utc::now();
        let expected = scheduler.schedule_new(now).unwrap();

        for (rating, want) in [
            (Rating::Again, expected.again.stability),
            (Rating::Hard, expected.hard.stability),
            (Rating::Good, expected.good.stability),
            (Rating::Easy, expected.easy.stability),
        ] {
            let states = scheduler.schedule_new(now).unwrap();
            assert_eq!(
                state_for_rating(states, rating).stability,
                want,
                "wrong state for {rating:?}"
            );
        }

        // And they really are distinct, so the assertions above can fail.
        assert_ne!(expected.again.stability, expected.easy.stability);
    }

    #[test]
    fn an_interval_shorter_than_a_day_still_counts_as_one() {
        // num_days() truncates. Without the floor, a sub-day interval is stored
        // as 0 and the card becomes permanently due — the schedule stops
        // spacing anything at all.
        let now = Utc::now();
        assert_eq!(interval_days_between(now, now), 1);
        assert_eq!(interval_days_between(now + Duration::hours(23), now), 1);
        assert_eq!(interval_days_between(now + Duration::days(5), now), 5);
        // Also guards against a due date in the past producing a negative.
        assert_eq!(interval_days_between(now - Duration::days(3), now), 1);
    }

    // -- against the real database (see handlers::test_db) ---------------------

    #[tokio::test]
    #[ignore = "requires the local Postgres cluster"]
    async fn reviews_prior_state_lookup_returns_the_latest_review() {
        let pool = test_db::pool().await;
        let mut tx = pool.begin().await.unwrap();
        let (user_id, set_id) = test_db::seed_learner(&mut tx).await;
        let card = test_db::seed_card(&mut tx, set_id, "reviewed twice").await;

        let now = Utc::now();
        test_db::record_review(&mut tx, card, user_id, 9.0, 2.0, now, now - Duration::days(2)).await;
        test_db::record_review(&mut tx, card, user_id, 0.5, 8.0, now, now - Duration::days(1)).await;

        let prior: Option<PriorEventRow> = sqlx::query_as(PRIOR_STATE_QUERY)
            .bind(card)
            .bind(user_id)
            .fetch_optional(&mut *tx)
            .await
            .unwrap();

        let prior = prior.expect("a reviewed card has prior state");
        // This row is the input to the next FSRS computation. Reading the first
        // review instead of the latest would reschedule the card from its
        // original state every single time, undoing the learner's history.
        assert_eq!(prior.stability, Some(0.5), "took the wrong review");
        assert_eq!(prior.difficulty, Some(8.0));
    }

    #[tokio::test]
    #[ignore = "requires the local Postgres cluster"]
    async fn reviews_a_card_with_no_history_has_no_prior_state() {
        let pool = test_db::pool().await;
        let mut tx = pool.begin().await.unwrap();
        let (user_id, set_id) = test_db::seed_learner(&mut tx).await;
        let card = test_db::seed_card(&mut tx, set_id, "never reviewed").await;

        let prior: Option<PriorEventRow> = sqlx::query_as(PRIOR_STATE_QUERY)
            .bind(card)
            .bind(user_id)
            .fetch_optional(&mut *tx)
            .await
            .unwrap();

        // None is what routes the handler to schedule_new rather than
        // schedule_review; an error here would be a 500 on a first review.
        assert!(prior.is_none());
    }

    #[tokio::test]
    #[ignore = "requires the local Postgres cluster"]
    async fn reviews_a_card_that_does_not_exist_is_rejected_as_a_bad_request() {
        let pool = test_db::pool().await;
        let mut tx = pool.begin().await.unwrap();
        let (user_id, _set_id) = test_db::seed_learner(&mut tx).await;

        let missing_card: Uuid = sqlx::query_scalar("SELECT gen_random_uuid()")
            .fetch_one(&mut *tx)
            .await
            .unwrap();

        let err = sqlx::query_as::<_, InsertedEventRow>(INSERT_EVENT_QUERY)
            .bind(missing_card)
            .bind(user_id)
            .bind(true)
            .bind(1.0_f64)
            .bind(5.0_f64)
            .bind(1_i32)
            .bind(Utc::now())
            .fetch_one(&mut *tx)
            .await
            .expect_err("a review for a card that does not exist must not be stored");

        // There is no existence check before the insert — the foreign key is
        // what catches this, and classify_db_error turns it into 400, not 404.
        // Recorded because the distinction is not obvious from reading the
        // handler: nothing in it mentions a missing card.
        let (status, message) = crate::handlers::classify_db_error(&err);
        assert_eq!(status, actix_web::http::StatusCode::BAD_REQUEST);
        assert!(message.contains("foreign key"), "message: {message}");
    }
}
