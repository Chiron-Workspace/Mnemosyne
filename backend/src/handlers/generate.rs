//! `POST /study_sets/{set_id}/generate_cards` — AI question generation.
//!
//! Sends user-provided source text to DeepSeek, parses generated Q&A pairs
//! into real cards, and logs the interaction in `ai_interactions` for cost
//! tracking. This is the first AI-integrated endpoint.

use actix_web::{post, web, HttpResponse};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgPool};
use uuid::Uuid;
use chrono::{DateTime, Utc};

use crate::deepseek::{DeepSeekClient, DeepSeekMessage, DEFAULT_MODEL};
use super::error_response;

/// Hard cap on source text length to keep token cost predictable and bounded.
/// Roughly 1.5-2K tokens of input at average English density — combined with
/// the n=5 default, total interaction cost stays well under 5K tokens.
const MAX_SOURCE_TEXT_CHARS: usize = 8000;

/// Default and clamp ceiling for `num_questions`.
const DEFAULT_NUM_QUESTIONS: u32 = 5;
const MAX_NUM_QUESTIONS: u32 = 10;

#[derive(Debug, Deserialize)]
pub struct GenerateCardsRequest {
    pub source_text: String,
    pub num_questions: Option<u32>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
struct GeneratedQAPair {
    question: String,
    answer: String,
}

/// One card row returned in the 201 response (mirrors `cards::CardRow`).
#[derive(Debug, Serialize, FromRow)]
struct CreatedCard {
    id: Uuid,
    set_id: Uuid,
    question: String,
    answer: String,
    created_at: DateTime<Utc>,
}

/// Row for logging the AI interaction. We insert with the study set's
/// `user_id` (looked up when validating set_id) since the schema's
/// `ai_interactions.user_id` is NOT NULL.
#[derive(Debug, FromRow)]
struct StudySetOwnerRow {
    user_id: Uuid,
}

/// Response shape returned on 201 — what got created plus a cost stub.
#[derive(Debug, Serialize)]
struct GenerateCardsResponse {
    cards: Vec<CreatedCard>,
    tokens_used: u32,
}

#[post("/study_sets/{set_id}/generate_cards")]
pub async fn generate_cards(
    pool: web::Data<PgPool>,
    deepseek: web::Data<DeepSeekClient>,
    path: web::Path<Uuid>,
    body: web::Json<GenerateCardsRequest>,
) -> HttpResponse {
    let set_id = path.into_inner();

    // 1. Validate num_questions. Silent clamp above MAX, reject 0 with 400
    //    (no point generating zero questions).
    let mut num_questions = body.num_questions.unwrap_or(DEFAULT_NUM_QUESTIONS);
    if num_questions == 0 {
        return error_response(
            actix_web::http::StatusCode::BAD_REQUEST,
            "num_questions must be >= 1",
        );
    }
    if num_questions > MAX_NUM_QUESTIONS {
        num_questions = MAX_NUM_QUESTIONS;
    }

    // 2. Validate source_text. Empty or whitespace-only is rejected; too long
    //    is rejected with the exact limit named so callers can fix on the
    //    client side.
    let trimmed = body.source_text.trim();
    if trimmed.is_empty() {
        return error_response(
            actix_web::http::StatusCode::BAD_REQUEST,
            "source_text must not be empty",
        );
    }
    if body.source_text.chars().count() > MAX_SOURCE_TEXT_CHARS {
        return error_response(
            actix_web::http::StatusCode::BAD_REQUEST,
            format!("source_text exceeds {MAX_SOURCE_TEXT_CHARS} character limit"),
        );
    }

    // 3. Validate set_id exists AND fetch its owning user_id (needed for the
    //    ai_interactions row, whose user_id is NOT NULL with FK to users).
    let owner: Option<StudySetOwnerRow> = match sqlx::query_as::<_, StudySetOwnerRow>(
        "SELECT user_id FROM study_sets WHERE id = $1",
    )
    .persistent(false)
    .bind(set_id)
    .fetch_optional(pool.get_ref())
    .await
    {
        Ok(r) => r,
        Err(e) => {
            return error_response(
                actix_web::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("database error looking up study set: {e}"),
            );
        }
    };
    let Some(owner) = owner else {
        return error_response(
            actix_web::http::StatusCode::NOT_FOUND,
            format!("study set {set_id} not found"),
        );
    };

    // 4. Build the prompt. Two messages: a strict system instruction that
    //    constrains output to pure JSON, and the user message carrying the
    //    source text and the requested count.
    let system_prompt = format!(
        "You are a flashcard author. Generate exactly {n} question/answer pairs \
         testing UNDERSTANDING of the source text (active recall: questions that \
         require retrieving or applying information, not copy-pasting sentences \
         back). Respond with ONLY valid JSON: an array of objects, each shaped \
         exactly like {{\"question\": string, \"answer\": string}}. No prose, no \
         markdown, no code fences, no commentary outside the JSON. The JSON must \
         parse with `serde_json::from_str::<Vec<QAPair>>` directly.",
        n = num_questions
    );
    let user_prompt = format!("Source text:\n\n{trimmed}\n\nGenerate {num_questions} pairs.");
    let messages = vec![
        DeepSeekMessage::system(system_prompt.clone()),
        DeepSeekMessage::user(user_prompt.clone()),
    ];

    // 5. Call DeepSeek.
    let outcome: Result<crate::deepseek::DeepSeekResponse, crate::deepseek::DeepSeekError> =
        deepseek.chat_completion(&messages, Some(DEFAULT_MODEL)).await;

    // We always log the ai_interactions row, in success OR failure. The
    // "input_text" we log is the literal prompt sent (system + user joined),
    // preserving what the assistant saw for debugging and cost attribution.
    let prompt_log = format!("[system] {system_prompt}\n[user] {user_prompt}");

    match outcome {
        Ok(resp) => {
            let raw = resp
                .choices
                .first()
                .map(|c| c.message.content.clone())
                .unwrap_or_default();

            // 6. Defensive parse: try the raw text, then strip a leading
            //    ```json fence if present.
            let cards_to_insert = match parse_qa_pairs(&raw) {
                Ok(pairs) => pairs,
                Err(parse_err) => {
                    // Generation succeeded but response wasn't JSON-parseable.
                    // Log the raw output_text so a human can diagnose, then 502.
                    let _ = log_ai_interaction(
                        pool.get_ref(),
                        owner.user_id,
                        &prompt_log,
                        &raw,
                        resp.usage.total_tokens,
                    )
                    .await;
                    return error_response(
                        actix_web::http::StatusCode::BAD_GATEWAY,
                        format!("DeepSeek returned non-JSON output, parse failed: {parse_err}"),
                    );
                }
            };

            // Partial parse decision: REJECT the whole batch if ANY pair is
            // missing `question` or `answer` or has empty strings. Rationale
            // documented in the report — atomic over silent partial inserts.
            let mut validated: Vec<GeneratedQAPair> = Vec::new();
            for (i, p) in cards_to_insert.iter().enumerate() {
                if p.question.trim().is_empty() || p.answer.trim().is_empty() {
                    let _ = log_ai_interaction(
                        pool.get_ref(),
                        owner.user_id,
                        &prompt_log,
                        &raw,
                        resp.usage.total_tokens,
                    )
                    .await;
                    return error_response(
                        actix_web::http::StatusCode::BAD_GATEWAY,
                        format!(
                            "DeepSeek returned a malformed pair at index {i} (empty question or answer). \
                             Batch rejected; no cards inserted."
                        ),
                    );
                }
                validated.push(p.clone());
            }

            if validated.is_empty() {
                let _ = log_ai_interaction(
                    pool.get_ref(),
                    owner.user_id,
                    &prompt_log,
                    &raw,
                    resp.usage.total_tokens,
                )
                .await;
                return error_response(
                    actix_web::http::StatusCode::BAD_GATEWAY,
                    "DeepSeek returned an empty array; nothing to insert",
                );
            }

            // 7. Insert each card. Use a single transaction-free batched
            //    insert (each `INSERT` has `.persistent(false)` per docs/gotchas.md)
            //    and collect the returned rows. Stop on first error — if the
            //    pool blows up partway the client sees a clear 500 with the
            //    successful count surfaced in the error message.
            let mut created: Vec<CreatedCard> = Vec::with_capacity(validated.len());
            for p in &validated {
                match sqlx::query_as::<_, CreatedCard>(
                    r#"INSERT INTO cards (set_id, question, answer)
                       VALUES ($1, $2, $3)
                       RETURNING id, set_id, question, answer, created_at"#,
                )
                .persistent(false)
                .bind(set_id)
                .bind(&p.question)
                .bind(&p.answer)
                .fetch_one(pool.get_ref())
                .await
                {
                    Ok(row) => created.push(row),
                    Err(e) => {
                        // Even on partial DB-insert failure, log the AI
                        // interaction (the LLM call still happened and cost
                        // real money).
                        let _ = log_ai_interaction(
                            pool.get_ref(),
                            owner.user_id,
                            &prompt_log,
                            &raw,
                            resp.usage.total_tokens,
                        )
                        .await;
                        return error_response(
                            actix_web::http::StatusCode::INTERNAL_SERVER_ERROR,
                            format!(
                                "card insertion failed after {} of {} cards inserted: {e}",
                                created.len(),
                                validated.len()
                            ),
                        );
                    }
                }
            }

            // 8. Log the successful AI interaction.
            let _ = log_ai_interaction(
                pool.get_ref(),
                owner.user_id,
                &prompt_log,
                &raw,
                resp.usage.total_tokens,
            )
            .await;

            // 9. Respond with the created cards + the token count for cost
            //    transparency.
            HttpResponse::Created().json(GenerateCardsResponse {
                cards: created,
                tokens_used: resp.usage.total_tokens,
            })
        }
        Err(api_err) => {
            // DeepSeek call never succeeded (network, auth, rate limit). Log
            // the attempted input + a placeholder output that makes the
            // failure reason clear, with 0 tokens since we got nothing back.
            let placeholder =
                format!("[no response received from DeepSeek — call failed: {api_err}]");
            let _ = log_ai_interaction(
                pool.get_ref(),
                owner.user_id,
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

/// Strip a leading ```` ```json ```` (or ```` ``` ````) fence and trailing
/// ```` ``` ```` then attempt serde_json parse. Also tries the raw string
/// verbatim first, so unwrapped output still works.
fn parse_qa_pairs(raw: &str) -> Result<Vec<GeneratedQAPair>, String> {
    // First, try as-is.
    if let Ok(v) = serde_json::from_str::<Vec<GeneratedQAPair>>(raw) {
        return Ok(v);
    }

    // Strip a leading ```json or ``` fence and trailing ```.
    let trimmed = raw.trim();
    let stripped: &str = if trimmed.starts_with("```") {
        let after_open = trimmed
            .strip_prefix("```json")
            .or_else(|| trimmed.strip_prefix("```"))
            .unwrap_or(trimmed);
        if let Some(after) = after_open.strip_suffix("```") {
            after.trim()
        } else {
            after_open.trim()
        }
    } else {
        trimmed
    };

    serde_json::from_str::<Vec<GeneratedQAPair>>(stripped)
        .map_err(|e| format!("{e} (after stripping fences)"))
}

/// Insert a row into `ai_interactions`. Fire-and-forget: returns the error
/// (if any) wrapped in `Option` so the caller can decide whether to surface
/// it. In this handler, the AI interaction log is best-effort — we don't
/// fail the user-facing request if logging fails; we just lose telemetry.
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
           VALUES ($1, 'question_generation', $2, $3, $4)"#,
    )
    .persistent(false)
    .bind(user_id)
    .bind(input_text)
    .bind(output_text)
    .bind(tokens_used as i32)
    .execute(pool)
    .await
    .map(|_| ())
}