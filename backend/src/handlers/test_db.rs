//! Fixtures for tests that must run against a real database.
//!
//! Some behaviour in this crate lives in SQL rather than in Rust: which of a
//! card's reviews counts as current, which cards are withheld until their due
//! date, whose cards a learner can see. None of that can be checked with a
//! fake — a test that stubs the database only re-asserts the stub. So these
//! tests talk to the real Postgres.
//!
//! Every one of them runs inside a transaction that is **never committed**, so
//! a full run leaves the database exactly as it found it. Each seeds its own
//! learner rather than reusing whatever rows happen to exist, so results do not
//! depend on the state of the developer's machine.
//!
//! They are `#[ignore]` by default, matching the convention the live KS and
//! DeepSeek tests already follow: an offline `cargo test` must stay green
//! without a database. Run them deliberately:
//!
//!     cargo test -p backend -- --ignored due_ reviews_

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

/// Connect using the same `DATABASE_URL` the server uses.
pub async fn pool() -> PgPool {
    dotenvy::dotenv().ok();
    dotenvy::from_filename("../.env").ok();
    let url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL must be set (see .env.example) to run the database tests");
    PgPool::connect(&url)
        .await
        .expect("the database tests need the local Postgres cluster running")
}

/// A learner with one study set, both freshly created inside the transaction.
pub async fn seed_learner(conn: &mut sqlx::PgConnection) -> (Uuid, Uuid) {
    // Email is unique in the schema, and the generator lives in Postgres
    // because the `uuid` crate is built here without its v4 feature.
    let user_id: Uuid = sqlx::query_scalar(
        "INSERT INTO users (email) VALUES ('test-' || gen_random_uuid() || '@example.invalid') \
         RETURNING id",
    )
    .fetch_one(&mut *conn)
    .await
    .expect("seeding a user should succeed");

    let set_id: Uuid = sqlx::query_scalar(
        "INSERT INTO study_sets (user_id, name, topic) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(user_id)
    .bind("test set")
    .bind("testing")
    .fetch_one(&mut *conn)
    .await
    .expect("seeding a study set should succeed");

    (user_id, set_id)
}

pub async fn seed_card(conn: &mut sqlx::PgConnection, set_id: Uuid, question: &str) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO cards (set_id, question, answer) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(set_id)
    .bind(question)
    .bind("an answer")
    .fetch_one(&mut *conn)
    .await
    .expect("seeding a card should succeed")
}

/// Record one review. `created_at` is set explicitly rather than left to
/// `now()`, because which row counts as the latest is exactly what several of
/// these tests are about.
#[allow(clippy::too_many_arguments)]
pub async fn record_review(
    conn: &mut sqlx::PgConnection,
    card_id: Uuid,
    user_id: Uuid,
    stability: f64,
    difficulty: f64,
    next_review_at: DateTime<Utc>,
    created_at: DateTime<Utc>,
) {
    sqlx::query(
        r#"INSERT INTO learning_events
             (card_id, user_id, is_correct, stability, difficulty, interval, next_review_at, created_at)
           VALUES ($1, $2, true, $3, $4, 1, $5, $6)"#,
    )
    .bind(card_id)
    .bind(user_id)
    .bind(stability)
    .bind(difficulty)
    .bind(next_review_at)
    .bind(created_at)
    .execute(&mut *conn)
    .await
    .expect("recording a review should succeed");
}
