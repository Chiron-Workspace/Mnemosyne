//! Handlers for the `study_sets` table.
//!
//! POST /study_sets  — create a study set for a user
//! GET  /study_sets  — list study sets, optionally filtered by ?user_id=<uuid>

use actix_web::{HttpResponse, post, get, web};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgPool};
use uuid::Uuid;
use chrono::{DateTime, Utc};

use super::{classify_db_error, error_response};

#[derive(Debug, Deserialize)]
pub struct CreateStudySetRequest {
    pub user_id: Uuid,
    pub name: String,
    pub topic: Option<String>,
}

#[derive(Debug, Serialize, FromRow)]
pub struct StudySetRow {
    pub id: Uuid,
    pub user_id: Uuid,
    pub name: String,
    pub topic: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
pub struct ListStudySetsQuery {
    pub user_id: Option<Uuid>,
}

#[post("/study_sets")]
pub async fn create_study_set(
    pool: web::Data<PgPool>,
    body: web::Json<CreateStudySetRequest>,
) -> HttpResponse {
    if body.name.trim().is_empty() {
        return error_response(
            actix_web::http::StatusCode::BAD_REQUEST,
            "study set name must not be empty",
        );
    }

    match sqlx::query_as::<_, StudySetRow>(
        r#"INSERT INTO study_sets (user_id, name, topic)
           VALUES ($1, $2, $3)
           RETURNING id, user_id, name, topic, created_at"#,
    )
    .persistent(false)
    .bind(body.user_id)
    .bind(&body.name)
    .bind(&body.topic)
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

#[get("/study_sets")]
pub async fn list_study_sets(
    pool: web::Data<PgPool>,
    query: web::Query<ListStudySetsQuery>,
) -> HttpResponse {
    let result = match query.user_id {
        Some(uid) => {
            sqlx::query_as::<_, StudySetRow>(
                r#"SELECT id, user_id, name, topic, created_at
                   FROM study_sets
                   WHERE user_id = $1
                   ORDER BY created_at"#,
            )
            .persistent(false)
            .bind(uid)
            .fetch_all(pool.get_ref())
            .await
        }
        None => {
            sqlx::query_as::<_, StudySetRow>(
                r#"SELECT id, user_id, name, topic, created_at
                   FROM study_sets
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