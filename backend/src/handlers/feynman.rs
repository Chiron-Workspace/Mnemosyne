//! Feynman Technique evaluation endpoints.
//!
//! - `POST /study_sets/{set_id}/feynman_evaluate`               — submit explanation, get AI evaluation
//! - `GET  /study_sets/{set_id}/feynman_evaluate/history`       — read a user's evaluation history
//!
//! The Feynman Technique: the student explains a topic in their own words, and
//! the AI evaluates clarity, completeness, and correctness — plus gives
//! feedback and improvement suggestions. Scores are persisted (not just
//! logged) so a user's progress can be tracked over time.

use actix_web::{get, post, web, HttpResponse};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

use crate::llm_provider::{LLMProvider, LLMMessage};
use super::error_response;

/// Cap on total card content included in the evaluation prompt.
const MAX_CARD_CONTEXT_CHARS: usize = 6000;

/// Cap on student explanation length. A Feynman explanation should be a few
/// paragraphs, not a wall of text — keep token cost bounded.
const MAX_EXPLANATION_CHARS: usize = 4000;

// ---------------------------------------------------------------------------
// Request / response types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct FeynmanEvaluateRequest {
    pub user_id: Uuid,
    pub explanation_text: String,
}

#[derive(Debug, Deserialize)]
pub struct HistoryQuery {
    pub user_id: Uuid,
}

#[derive(Debug, Serialize)]
pub struct FeynmanEvaluateResponse {
    pub evaluation_id: Uuid,
    pub clarity_score: i32,
    pub completeness_score: i32,
    pub correctness_score: i32,
    pub feedback: String,
    pub suggestions: String,
}

#[derive(Debug, Serialize)]
pub struct HistoryResponse {
    pub evaluations: Vec<HistoryEntry>,
    pub count: i64,
}

#[derive(Debug, Serialize, FromRow)]
pub struct HistoryEntry {
    pub id: Uuid,
    pub explanation_text: String,
    pub clarity_score: i32,
    pub completeness_score: i32,
    pub correctness_score: i32,
    pub feedback: String,
    pub suggestions: String,
    pub created_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// Internal DB row types
// ---------------------------------------------------------------------------

#[derive(Debug, FromRow)]
struct CardContentRow {
    question: String,
    answer: String,
}

#[derive(Debug, FromRow)]
struct InsertedEvaluationRow {
    id: Uuid,
}

// ---------------------------------------------------------------------------
// Structured-JSON parsing for the AI response
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Serialize)]
struct FeynmanAIResponse {
    clarity_score: i32,
    completeness_score: i32,
    correctness_score: i32,
    feedback: String,
    suggestions: String,
}

/// Defensive parse: try raw, then strip a leading ```json or ``` fence.
fn parse_feynman_response(raw: &str) -> Result<FeynmanAIResponse, String> {
    if let Ok(v) = serde_json::from_str::<FeynmanAIResponse>(raw) {
        return Ok(v);
    }
    let trimmed = raw.trim();
    let stripped: &str = if trimmed.starts_with("```") {
        let after = trimmed
            .strip_prefix("```json")
            .or_else(|| trimmed.strip_prefix("```"))
            .unwrap_or(trimmed);
        if let Some(s) = after.strip_suffix("```") {
            s.trim()
        } else {
            after.trim()
        }
    } else {
        trimmed
    };
    serde_json::from_str::<FeynmanAIResponse>(stripped)
        .map_err(|e| format!("{e} (after stripping fences)"))
}

// ---------------------------------------------------------------------------
// Card-context fetching (local copy — not shared with socratic.rs to keep
// modules independent; same 6000-char cap, same whole-card-granularity rule)
// ---------------------------------------------------------------------------

async fn fetch_card_context(pool: &PgPool, set_id: Uuid) -> Result<Option<String>, String> {
    let cards: Vec<CardContentRow> = sqlx::query_as::<_, CardContentRow>(
        "SELECT question, answer FROM cards WHERE set_id = $1 ORDER BY created_at",
    )
    .bind(set_id)
    .fetch_all(pool)
    .await
    .map_err(|e| format!("database error fetching cards: {e}"))?;

    if cards.is_empty() {
        return Ok(None);
    }

    let mut context = String::new();
    for card in &cards {
        let entry = format!("Q: {}\nA: {}\n\n", card.question, card.answer);
        if context.len() + entry.len() > MAX_CARD_CONTEXT_CHARS {
            break;
        }
        context.push_str(&entry);
    }
    Ok(Some(context))
}

// ---------------------------------------------------------------------------
// Prompt construction
// ---------------------------------------------------------------------------

fn build_system_prompt(card_context: &str) -> String {
    format!(
        "You are a Feynman Technique evaluator. The student has written an \
         explanation of the following material IN THEIR OWN WORDS. Your job is \
         to evaluate their explanation critically and fairly.\n\
         \n\
         The material the student is explaining:\n\
         {card_context}\n\
         \n\
         Evaluate the student's explanation on three dimensions, each scored 1-10:\n\
         - clarity_score: Is the explanation understandable? Does it avoid unnecessary \
         jargon? Is it well-organized?\n\
         - completeness_score: Does it cover the KEY ideas from the material, or does \
         it leave out important parts?\n\
         - correctness_score: Is it factually accurate? Does it contain any \
         misconceptions or errors?\n\
         \n\
         Also provide:\n\
         - feedback: 2-4 sentences of overall feedback. Be genuinely critical if \
         warranted — do not inflate praise. If the explanation is weak, say so.\n\
         - suggestions: 2-4 sentences of specific, actionable suggestions for how the \
         student can improve their explanation.\n\
         \n\
         Respond with ONLY valid JSON: \
         {{\"clarity_score\": int, \"completeness_score\": int, \
         \"correctness_score\": int, \"feedback\": string, \"suggestions\": string}}. \
         No prose, no markdown fences, no commentary outside the JSON.",
    )
}

// ---------------------------------------------------------------------------
// Endpoints
// ---------------------------------------------------------------------------

#[post("/study_sets/{set_id}/feynman_evaluate")]
pub async fn evaluate(
    pool: web::Data<PgPool>,
    llm: web::Data<Box<dyn LLMProvider>>,
    path: web::Path<Uuid>,
    body: web::Json<FeynmanEvaluateRequest>,
) -> HttpResponse {
    let set_id = path.into_inner();

    // 1. Validate set_id exists.
    let set_exists: bool = match sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM study_sets WHERE id = $1)",
    )
    .bind(set_id)
    .fetch_one(pool.get_ref())
    .await
    {
        Ok(b) => b,
        Err(e) => {
            return error_response(
                actix_web::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("database error: {e}"),
            );
        }
    };
    if !set_exists {
        return error_response(
            actix_web::http::StatusCode::NOT_FOUND,
            format!("study set {set_id} not found"),
        );
    }

    // 2. Validate explanation_text: non-empty, under cap.
    let trimmed = body.explanation_text.trim();
    if trimmed.is_empty() {
        return error_response(
            actix_web::http::StatusCode::BAD_REQUEST,
            "explanation_text must not be empty",
        );
    }
    if body.explanation_text.chars().count() > MAX_EXPLANATION_CHARS {
        return error_response(
            actix_web::http::StatusCode::BAD_REQUEST,
            format!("explanation_text exceeds {MAX_EXPLANATION_CHARS} character limit"),
        );
    }

    // 3. Fetch card context. Zero cards → 400.
    let card_context = match fetch_card_context(pool.get_ref(), set_id).await {
        Ok(Some(c)) => c,
        Ok(None) => {
            return error_response(
                actix_web::http::StatusCode::BAD_REQUEST,
                "study set has no cards — nothing to evaluate against, generate some cards first",
            );
        }
        Err(e) => {
            return error_response(
                actix_web::http::StatusCode::INTERNAL_SERVER_ERROR,
                e,
            );
        }
    };

    // 4. Build prompt.
    let system_prompt = build_system_prompt(&card_context);
    let user_prompt = format!(
        "Student's explanation:\n\n{trimmed}\n\nEvaluate this explanation."
    );
    let messages = vec![
        LLMMessage::system(system_prompt.clone()),
        LLMMessage::user(user_prompt.clone()),
    ];
    let prompt_log =
        format!("[feynman:evaluate]\n[system] {system_prompt}\n[user] {user_prompt}");

    // 5. Call LLM provider.
    match llm.chat_completion(&messages, None).await {
        Ok(resp) => {
            let raw = resp.content;

            // 6. Parse structured JSON.
            let parsed = match parse_feynman_response(&raw) {
                Ok(p) => p,
                Err(parse_err) => {
                    let _ = log_ai_interaction(
                        pool.get_ref(),
                        body.user_id,
                        &prompt_log,
                        &raw,
                        resp.total_tokens,
                    )
                    .await;
                    return error_response(
                        actix_web::http::StatusCode::BAD_GATEWAY,
                        format!("DeepSeek returned non-JSON output, parse failed: {parse_err}"),
                    );
                }
            };

            // 7. Validate scores in application code (Decision 3) — before the
            //    DB CHECK constraint catches them. A 502 with a clear message
            //    is better UX than a raw constraint-violation 500.
            let validate_score = |s: i32, name: &str| -> Result<(), String> {
                if !(1..=10).contains(&s) {
                    Err(format!(
                        "AI returned {name}={s} (must be 1-10)"
                    ))
                } else {
                    Ok(())
                }
            };
            if let Err(e) = validate_score(parsed.clarity_score, "clarity_score")
                .and_then(|()| validate_score(parsed.completeness_score, "completeness_score"))
                .and_then(|()| validate_score(parsed.correctness_score, "correctness_score"))
            {
                let _ = log_ai_interaction(
                    pool.get_ref(),
                    body.user_id,
                    &prompt_log,
                    &raw,
                    resp.total_tokens,
                )
                .await;
                return error_response(
                    actix_web::http::StatusCode::BAD_GATEWAY,
                    format!("AI returned an invalid score: {e}"),
                );
            }

            // 8. Insert into feynman_evaluations.
            let inserted: InsertedEvaluationRow = match sqlx::query_as::<_, InsertedEvaluationRow>(
                r#"INSERT INTO feynman_evaluations
                     (user_id, set_id, explanation_text,
                      clarity_score, completeness_score, correctness_score,
                      feedback, suggestions)
                   VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
                   RETURNING id"#,
            )
            .bind(body.user_id)
            .bind(set_id)
            .bind(&body.explanation_text)
            .bind(parsed.clarity_score)
            .bind(parsed.completeness_score)
            .bind(parsed.correctness_score)
            .bind(&parsed.feedback)
            .bind(&parsed.suggestions)
            .fetch_one(pool.get_ref())
            .await
            {
                Ok(r) => r,
                Err(e) => {
                    let _ = log_ai_interaction(
                        pool.get_ref(),
                        body.user_id,
                        &prompt_log,
                        &raw,
                        resp.total_tokens,
                    )
                    .await;
                    return error_response(
                        actix_web::http::StatusCode::INTERNAL_SERVER_ERROR,
                        format!("failed to persist evaluation: {e}"),
                    );
                }
            };

            // 9. Log the AI interaction.
            let _ = log_ai_interaction(
                pool.get_ref(),
                body.user_id,
                &prompt_log,
                &raw,
                resp.total_tokens,
            )
            .await;

            // 10. Respond.
            HttpResponse::Created().json(FeynmanEvaluateResponse {
                evaluation_id: inserted.id,
                clarity_score: parsed.clarity_score,
                completeness_score: parsed.completeness_score,
                correctness_score: parsed.correctness_score,
                feedback: parsed.feedback,
                suggestions: parsed.suggestions,
            })
        }
        Err(api_err) => {
            let placeholder =
                format!("[no response received from DeepSeek — call failed: {api_err}]");
            let _ = log_ai_interaction(
                pool.get_ref(),
                body.user_id,
                &prompt_log,
                &placeholder,
                0,
            )
            .await;
            error_response(
                actix_web::http::StatusCode::BAD_GATEWAY,
                format!("DeepSeek API call failed: {api_err}"),
            )
        }
    }
}

#[get("/study_sets/{set_id}/feynman_evaluate/history")]
pub async fn history(
    pool: web::Data<PgPool>,
    path: web::Path<Uuid>,
    query: web::Query<HistoryQuery>,
) -> HttpResponse {
    let set_id = path.into_inner();

    let evaluations: Vec<HistoryEntry> = match sqlx::query_as::<_, HistoryEntry>(
        r#"SELECT id, explanation_text, clarity_score, completeness_score,
                  correctness_score, feedback, suggestions, created_at
           FROM feynman_evaluations
           WHERE set_id = $1 AND user_id = $2
           ORDER BY created_at ASC"#,
    )
    .bind(set_id)
    .bind(query.user_id)
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

    let count = evaluations.len() as i64;
    HttpResponse::Ok().json(HistoryResponse {
        evaluations,
        count,
    })
}

// ---------------------------------------------------------------------------
// Shared helper: log to ai_interactions
// ---------------------------------------------------------------------------

async fn log_ai_interaction(
    pool: &PgPool,
    user_id: Uuid,
    input_text: &str,
    output_text: &str,
    tokens_used: u32,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"INSERT INTO ai_interactions
             (user_id, interaction_type, input_text, output_text, tokens_used)
           VALUES ($1, 'feynman_evaluation', $2, $3, $4)"#,
    )
    .bind(user_id)
    .bind(input_text)
    .bind(output_text)
    .bind(tokens_used as i32)
    .execute(pool)
    .await
    .map(|_| ())
}