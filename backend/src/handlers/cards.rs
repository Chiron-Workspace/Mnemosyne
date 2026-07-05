//! Handlers for the `cards` table.
//!
//! POST /cards  — create a flashcard in a study set
//! GET  /cards  — list cards, optionally filtered by ?set_id=<uuid>

use actix_web::{HttpResponse, post, get, web};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgPool};
use uuid::Uuid;
use chrono::{DateTime, Utc};

use super::{classify_db_error, error_response};

#[derive(Debug, Deserialize)]
pub struct CreateCardRequest {
    pub set_id: Uuid,
    pub question: String,
    pub answer: String,
}

#[derive(Debug, Serialize, FromRow)]
pub struct CardRow {
    pub id: Uuid,
    pub set_id: Uuid,
    pub question: String,
    pub answer: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
pub struct ListCardsQuery {
    pub set_id: Option<Uuid>,
}

#[post("/cards")]
pub async fn create_card(
    pool: web::Data<PgPool>,
    body: web::Json<CreateCardRequest>,
) -> HttpResponse {
    if body.question.trim().is_empty() || body.answer.trim().is_empty() {
        return error_response(
            actix_web::http::StatusCode::BAD_REQUEST,
            "question and answer must not be empty",
        );
    }

    match sqlx::query_as::<_, CardRow>(
        r#"INSERT INTO cards (set_id, question, answer)
           VALUES ($1, $2, $3)
           RETURNING id, set_id, question, answer, created_at"#,
    )
    .persistent(false)
    .bind(body.set_id)
    .bind(&body.question)
    .bind(&body.answer)
    .fetch_one(pool.get_ref())
    .await
    {
        Ok(row) => HttpResponse::Created().json(row),
        Err(e) => {
            let (status, msg) = classify_db_error(&e);
            error_response(status, msg)
        }
    }
}

#[get("/cards")]
pub async fn list_cards(
    pool: web::Data<PgPool>,
    query: web::Query<ListCardsQuery>,
) -> HttpResponse {
    let result = match query.set_id {
        Some(sid) => {
            sqlx::query_as::<_, CardRow>(
                r#"SELECT id, set_id, question, answer, created_at
                   FROM cards
                   WHERE set_id = $1
                   ORDER BY created_at"#,
            )
            .persistent(false)
            .bind(sid)
            .fetch_all(pool.get_ref())
            .await
        }
        None => {
            sqlx::query_as::<_, CardRow>(
                r#"SELECT id, set_id, question, answer, created_at
                   FROM cards
                   ORDER BY created_at"#,
            )
            .persistent(false)
            .fetch_all(pool.get_ref())
            .await
        }
    };

    match result {
        Ok(rows) => HttpResponse::Ok().json(rows),
        Err(e) => error_response(
            actix_web::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("database error: {e}"),
        ),
    }
}