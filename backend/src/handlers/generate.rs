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

use crate::llm_provider::{LLMProvider, LLMMessage};
use super::{describe_llm_failure, error_response};

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
    /// Optional style control — "recall" (1-5 word answers, active-recall SAFMEDS)
    /// or "elaboration" (deeper 1-3 sentence Feynman-style answers). Defaults to
    /// "recall" when omitted. Rejected with 400 for any other value.
    #[serde(default = "default_style")]
    pub question_style: String,
}

fn default_style() -> String {
    "recall".to_string()
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
    llm: web::Data<Box<dyn LLMProvider>>,
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

    // 2b. Validate question_style. Only "recall" and "elaboration" are accepted;
    //    anything else (including empty string when not defaulted) is rejected.
    let style = body.question_style.trim().to_lowercase();
    if style != "recall" && style != "elaboration" {
        return error_response(
            actix_web::http::StatusCode::BAD_REQUEST,
            format!(
                "invalid question_style '{0}': must be 'recall' or 'elaboration'",
                body.question_style
            ),
        );
    }

    // 4. Build the prompt. Two distinct prompt strategies share the output-
    //    format constraint (pure JSON array of {question, answer}) but differ
    //    in how they instruct the model about content.
    let is_recall = style == "recall";

    let (system_prompt, user_prompt) = if is_recall {
        build_recall_prompt(num_questions, trimmed)
    } else {
        build_elaboration_prompt(num_questions, trimmed)
    };

    let messages = vec![
        LLMMessage::system(system_prompt.clone()),
        LLMMessage::user(user_prompt.clone()),
    ];

    // We always log the ai_interactions row, in success OR failure. The
    // "input_text" we log is the literal prompt sent — system + user joined,
    // prefixed with a style tag so any query of ai_interactions later can
    // distinguish which mode produced each interaction without needing a new
    // schema column.
    let prompt_log = format!("[style: {style}]\n[system] {system_prompt}\n[user] {user_prompt}");

    // 5. Call LLM provider.
    let outcome = llm.chat_completion(&messages, None).await;

    match outcome {
        Ok(resp) => {
            let raw = resp.content;

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
                        resp.total_tokens,
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
                        resp.total_tokens,
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
                    resp.total_tokens,
                )
                .await;
                return error_response(
                    actix_web::http::StatusCode::BAD_GATEWAY,
                    "DeepSeek returned an empty array; nothing to insert",
                );
            }

            // 7. Insert each card. Use a single transaction-free batched
            //    insert and collect the returned rows. Stop on first error — if the
            //    pool blows up partway the client sees a clear 500 with the
            //    successful count surfaced in the error message.
            let mut created: Vec<CreatedCard> = Vec::with_capacity(validated.len());
            for p in &validated {
                // source='topic': these were written by the model from free
                // text the user supplied, which is a different provenance from
                // a hand-typed card (POST /cards, 'manual') and from one built
                // out of a Knowledge Store concept ('knowledge_store'). Stamped
                // explicitly rather than left to the column default, which
                // would file every generated card as hand-written.
                match sqlx::query_as::<_, CreatedCard>(
                    r#"INSERT INTO cards (set_id, question, answer, source)
                       VALUES ($1, $2, $3, 'topic')
                       RETURNING id, set_id, question, answer, created_at"#,
                )
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
                            resp.total_tokens,
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
                resp.total_tokens,
            )
            .await;

            // 9. Respond with the created cards + the token count for cost
            //    transparency.
            HttpResponse::Created().json(GenerateCardsResponse {
                cards: created,
                tokens_used: resp.total_tokens,
            })
        }
        Err(api_err) => {
            // DeepSeek call never succeeded (network, auth, rate limit). Log
            // the attempted input + a placeholder output that makes the
            // failure reason clear, with 0 tokens since we got nothing back.
            let failure = describe_llm_failure(&api_err);
            let _ = log_ai_interaction(
                pool.get_ref(),
                owner.user_id,
                &prompt_log,
                &failure.placeholder,
                failure.tokens_used,
            )
            .await;
            error_response(actix_web::http::StatusCode::BAD_GATEWAY, failure.message)
        }
    }
}

/// Build a **recall**-style prompt (SAFMEDS — short factual active recall).
///
/// The system prompt includes two concrete few-shot examples so the model has
/// a pattern to imitate, not just a rule to follow. Answers should be 1-5
/// words: a fact, name, date, number, or short phrase — NOT a sentence.
fn build_recall_prompt(num_questions: u32, source: &str) -> (String, String) {
    let system = format!(
        "You are a SAFMEDS-style flashcard author. Generate exactly {n} \
         question/answer pairs from the provided source text that test FACTUAL \
         RECALL — who, what, when, where, how many, define, name. Each answer \
         MUST be a SHORT, single fact: 1-5 words (a name, date, number, term, \
         or short phrase). Do NOT write sentences or explanations.\n\
         \n\
         Example format (real pairs you should imitate):\n\
         [{{\"question\": \"What year did Columbus first reach the Caribbean?\", \
         \"answer\": \"1492\"}},\n\
         {{\"question\": \"Which disease devastated Native American populations \
         during the Columbian Exchange?\", \"answer\": \"smallpox\"}}]\n\
         \n\
         Respond with ONLY valid JSON — an array of exactly {n} objects, each \
         shaped {{\"question\": string, \"answer\": string}}. No prose, no \
         markdown fences, no commentary outside the JSON. The JSON must parse \
         with `serde_json::from_str::<Vec<QAPair>>` directly.",
        n = num_questions
    );
    let user = format!(
        "Source text:\n\n{source}\n\nGenerate {n} SAFMEDS-style short recall pairs.",
        n = num_questions
    );
    (system, user)
}

/// Build an **elaboration**-style prompt (Feynman / deep understanding).
///
/// Questions require synthesis, application, or explaining WHY/HOW. Answers
/// may be 1-3 sentences. This is the Prompt 4 style, tightened slightly to
/// reinforce the conceptual tone.
fn build_elaboration_prompt(num_questions: u32, source: &str) -> (String, String) {
    let system = format!(
        "You are a flashcard author. Generate exactly {n} question/answer pairs \
         that test DEEPER UNDERSTANDING of the source text. Questions should \
         require synthesizing information, explaining causal relationships, \
         applying concepts to new scenarios, or connecting ideas — NOT just \
         repeating isolated facts. Answers may be 1-3 sentences long.\n\
         \n\
         Respond with ONLY valid JSON: an array of exactly {n} objects, each \
         shaped {{\"question\": string, \"answer\": string}}. No prose, no \
         markdown fences, no commentary outside the JSON.",
        n = num_questions
    );
    let user = format!(
        "Source text:\n\n{source}\n\nGenerate {n} elaboration-style pairs.",
        n = num_questions
    );
    (system, user)
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
    .bind(user_id)
    .bind(input_text)
    .bind(output_text)
    .bind(tokens_used as i32)
    .execute(pool)
    .await
    .map(|_| ())
}