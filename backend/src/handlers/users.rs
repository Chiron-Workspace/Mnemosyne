//! Handlers for the `users` table.
//!
//! POST /users        — create a user
//! GET  /users        — list all users

use actix_web::{HttpResponse, post, get, web};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgPool};
use uuid::Uuid;
use chrono::{DateTime, Utc};

use super::{classify_db_error, error_response};

#[derive(Debug, Deserialize)]
pub struct CreateUserRequest {
    pub email: String,
    pub learning_style: Option<String>,
}

#[derive(Debug, Serialize, FromRow)]
pub struct UserRow {
    pub id: Uuid,
    pub email: String,
    pub learning_style: Option<String>,
    pub created_at: DateTime<Utc>,
    // intentionally omitting updated_at from the response — we never update
    // learning_style in this prompt, so created_at is the meaningful timestamp.
}

#[post("/users")]
pub async fn create_user(
    pool: web::Data<PgPool>,
    body: web::Json<CreateUserRequest>,
) -> HttpResponse {
    if body.email.trim().is_empty() {
        return error_response(
            actix_web::http::StatusCode::BAD_REQUEST,
            "email must not be empty",
        );
    }

    match sqlx::query_as::<_, UserRow>(
        r#"INSERT INTO users (email, learning_style)
           VALUES ($1, $2)
           RETURNING id, email, learning_style, created_at"#,
    )
    .persistent(false)
    .bind(&body.email)
    .bind(&body.learning_style)
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

#[get("/users")]
pub async fn list_users(pool: web::Data<PgPool>) -> HttpResponse {
    match sqlx::query_as::<_, UserRow>(
        r#"SELECT id, email, learning_style, created_at FROM users ORDER BY created_at"#,
    )
    .persistent(false)
    .fetch_all(pool.get_ref())
    .await
    {
        Ok(rows) => HttpResponse::Ok().json(rows),
        Err(e) => error_response(
            actix_web::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("database error: {e}"),
        ),
    }
}