//! Socratic tutor dialogue endpoints.
//!
//! - `POST /socratic/start`           — begin a new dialogue session
//! - `POST /socratic/{id}/reply`      — send a student reply, get AI response
//! - `POST /socratic/{id}/end`        — close the session, ship transcript to KS
//! - `GET  /socratic/{id}`            — read full conversation history
//!
//! The Socratic method: the AI asks guiding questions, does NOT give direct
//! answers, and helps the student arrive at understanding themselves. Card
//! content from the study set provides the source material the tutor draws
//! from.

use actix_web::{get, post, web, HttpResponse};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

use crate::ks_client::{KsClient, SaveTranscriptOutcome, TranscriptTurn};
use crate::llm_provider::{LLMProvider, LLMMessage};
use super::error_response;

/// Cap on total card content (Q+A text) included in the system prompt to
/// keep token cost bounded. ~6000 chars ≈ 1.5K tokens of context.
const MAX_CARD_CONTEXT_CHARS: usize = 6000;

/// Turn-cap guardrail: if a session already has this many messages (rows in
/// socratic_messages), reject further replies. 40 rows = 20 user + 20
/// assistant turns.
const TURN_CAP: i64 = 40;

/// Sliding-window size: only the most recent N messages are sent to DeepSeek
/// on each /reply call, keeping token cost bounded as conversations grow.
const CONTEXT_WINDOW: usize = 20;

// ---------------------------------------------------------------------------
// Request / response types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct StartRequest {
    pub study_set_id: Uuid,
    pub user_id: Uuid,
}

#[derive(Debug, Deserialize)]
pub struct ReplyRequest {
    pub message: String,
}

#[derive(Debug, Serialize)]
pub struct StartResponse {
    pub session_id: Uuid,
    pub opening_message: String,
}

#[derive(Debug, Serialize)]
pub struct ReplyResponse {
    pub reply: String,
    pub flagged_misconception: Option<String>,
}

/// What happened to the transcript hand-off when a session was closed.
/// Reported back to the caller for visibility only — none of these states
/// makes `/end` fail, because the study session is the real work and KS sync
/// is bookkeeping alongside it.
#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum KsSyncStatus {
    /// KS accepted the transcript (or already had it under this session_ref).
    Saved { transcript_id: String },
    /// No `KS_HTTP_TOKEN` configured — sync is switched off.
    Disabled,
    /// The session had no messages; there was nothing worth shipping.
    NothingToSend,
    /// KS answered but its own database is down. An immediate retry would not
    /// help; the transcript can be re-sent later under the same session_ref.
    KsDbUnavailable { error: String },
    /// Transport or protocol failure talking to KS.
    Failed { error: String },
}

#[derive(Debug, Serialize)]
pub struct EndResponse {
    pub session_id: Uuid,
    pub message_count: usize,
    pub knowledge_store: KsSyncStatus,
}

#[derive(Debug, Serialize)]
pub struct SessionHistoryResponse {
    pub session_id: Uuid,
    pub messages: Vec<MessageOut>,
}

#[derive(Debug, Serialize, FromRow)]
pub struct MessageOut {
    pub role: String,
    pub content: String,
    pub flagged_misconception: Option<String>,
    pub created_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// Internal DB row types
// ---------------------------------------------------------------------------

#[derive(Debug, FromRow)]
struct SessionRow {
    id: Uuid,
    user_id: Uuid,
    set_id: Uuid,
}

#[derive(Debug, FromRow)]
struct CardContentRow {
    question: String,
    answer: String,
}

#[derive(Debug, FromRow)]
struct MessageRow {
    role: String,
    content: String,
    flagged_misconception: Option<String>,
}

// (InsertedMessageRow removed — we don't need the returned id, just execute())

// ---------------------------------------------------------------------------
// Structured-JSON parsing for the AI response
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Serialize)]
struct SocraticAIResponse {
    reply: String,
    #[serde(default)]
    flagged_misconception: Option<String>,
}

/// Defensive parse: try raw, then strip a leading ```json or ``` fence.
fn parse_socratic_response(raw: &str) -> Result<SocraticAIResponse, String> {
    if let Ok(v) = serde_json::from_str::<SocraticAIResponse>(raw) {
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
    serde_json::from_str::<SocraticAIResponse>(stripped)
        .map_err(|e| format!("{e} (after stripping fences)"))
}

fn format_assistant_history_message(content: &str, flagged_misconception: Option<&str>) -> String {
    serde_json::to_string(&SocraticAIResponse {
        reply: content.to_owned(),
        flagged_misconception: flagged_misconception.map(str::to_owned),
    })
    .expect("serializing SocraticAIResponse for DeepSeek history should never fail")
}

// ---------------------------------------------------------------------------
// Prompt construction
// ---------------------------------------------------------------------------

/// Build the Socratic-tutor system prompt. Shared between /start and /reply
/// so the AI gets consistent instructions about what material it's tutoring.
/// Card content is re-fetched on every call (no caching) — for a 2-3 user
/// app the query cost is negligible, and re-fetching avoids stale-context
/// risk if cards are edited between turns.
fn build_system_prompt(card_context: &str) -> String {
    format!(
        "You are a Socratic tutor. Your goal is to help the student understand \
         the material through guided questioning — NOT by giving direct answers.\n\
         \n\
Rules:\n\
          1. Ask probing, guiding questions that lead the student to discover the \
          answer themselves.\n\
          2. If the student's answer is correct, affirm it briefly and move to a \
          deeper or related question.\n\
          3. If the student's answer contains a misconception, ask a question that \
          will help them see the error — do NOT simply state the correction.\n\
          4. If the student's answer does not address the question you actually \
          asked (off-topic, or answers a different question entirely), you MUST \
          explicitly say so in your reply, then restate the original question in \
          your own words and ask the student to answer THAT question. Do NOT \
          engage with, build on, or ask follow-up questions about the off-topic \
          content. Do NOT praise or affirm the off-topic answer. This rule takes \
          priority over rule 5 when the student's answer is off-topic.\n\
          5. Build on what the student says about the current question — refer to \
          their previous answers when they are relevant to the question at hand. \
          This does NOT mean following whatever topic the student introduces; if \
          they switch topics, apply rule 4 instead.\n\
          6. Do NOT lecture or give long explanations. Your messages should be \
          concise: ideally 1-4 sentences plus a question.\n\
         \n\
         The material this session covers:\n\
         {card_context}\n\
         \n\
         Respond with ONLY valid JSON: {{\"reply\": string, \
         \"flagged_misconception\": string | null}}. The `reply` field is your \
         message to the student. The `flagged_misconception` field is a short \
         description of any misconception you detected in the student's last \
         message, or null if the answer was correct or if this is the opening \
         question. This JSON-only requirement ALWAYS applies, even if the \
         student's message is off-topic, nonsensical, hostile, or clearly wrong. \
         No prose, no markdown fences, no commentary outside the JSON.",
    )
}

/// Fetch cards for a study set and concatenate Q+A into a bounded context
/// string. Caps at MAX_CARD_CONTEXT_CHARS, never truncating mid-card.
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
    for (i, card) in cards.iter().enumerate() {
        let entry = format!("Q: {}\nA: {}\n\n", card.question, card.answer);
        if context.len() + entry.len() > MAX_CARD_CONTEXT_CHARS {
            // Stop adding cards — the cap is reached. We include only whole
            // cards, never truncated mid-card.
            break;
        }
        context.push_str(&entry);
        let _ = i; // index unused but kept for potential debug logging
    }
    Ok(Some(context))
}

// ---------------------------------------------------------------------------
// Endpoints
// ---------------------------------------------------------------------------

#[post("/socratic/start")]
pub async fn start(
    pool: web::Data<PgPool>,
    llm: web::Data<Box<dyn LLMProvider>>,
    body: web::Json<StartRequest>,
) -> HttpResponse {
    // 1. Validate study_set exists.
    let set_exists: bool = match sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM study_sets WHERE id = $1)",
    )
    .bind(body.study_set_id)
    .fetch_one(pool.get_ref())
    .await
    {
        Ok(b) => b,
        Err(e) => {
            return error_response(
                actix_web::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("database error looking up study set: {e}"),
            );
        }
    };
    if !set_exists {
        return error_response(
            actix_web::http::StatusCode::NOT_FOUND,
            format!("study set {} not found", body.study_set_id),
        );
    }

    // 2. Fetch card context. Zero cards → 400 per Decision 2.
    let card_context = match fetch_card_context(pool.get_ref(), body.study_set_id).await {
        Ok(Some(c)) => c,
        Ok(None) => {
            return error_response(
                actix_web::http::StatusCode::BAD_REQUEST,
                "study set has no cards — nothing to discuss yet, generate some cards first",
            );
        }
        Err(e) => {
            return error_response(
                actix_web::http::StatusCode::INTERNAL_SERVER_ERROR,
                e,
            );
        }
    };

    // 3. Insert the socratic_sessions row.
    let session_row: SessionRow = match sqlx::query_as::<_, SessionRow>(
        r#"INSERT INTO socratic_sessions (user_id, set_id)
           VALUES ($1, $2)
           RETURNING id, user_id, set_id"#,
    )
    .bind(body.user_id)
    .bind(body.study_set_id)
    .fetch_one(pool.get_ref())
    .await
    {
        Ok(r) => r,
        Err(e) => {
            let msg = if let Some(code) = e.as_database_error().and_then(|e| e.code()) {
                if code == "23503" {
                    return error_response(
                        actix_web::http::StatusCode::BAD_REQUEST,
                        "user_id does not exist (foreign key violation)",
                    );
                }
                format!("database error: {e}")
            } else {
                format!("database error: {e}")
            };
            return error_response(
                actix_web::http::StatusCode::INTERNAL_SERVER_ERROR,
                msg,
            );
        }
    };

    // 4. Build system prompt + a user message initiating the session.
    let system_prompt = build_system_prompt(&card_context);
    let messages = vec![
        LLMMessage::system(system_prompt.clone()),
        LLMMessage::user("Begin the session. Ask ONE opening question."),
    ];

    // 5. Call LLM provider.
    let prompt_log = format!("[socratic:start]\n[system] {system_prompt}\n[user] Begin the session.");

    match llm.chat_completion(&messages, None).await {
        Ok(resp) => {
            let raw = resp.content;

            // 6. Parse structured JSON.
            let parsed = match parse_socratic_response(&raw) {
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

            // 7. Insert the assistant's opening message.
            let _ = sqlx::query(
                r#"INSERT INTO socratic_messages
                     (session_id, role, content, flagged_misconception)
                   VALUES ($1, 'assistant', $2, $3)"#,
            )
            .bind(session_row.id)
            .bind(&parsed.reply)
            .bind(&parsed.flagged_misconception)
            .execute(pool.get_ref())
            .await;

            // 8. Log the AI interaction.
            let _ = log_ai_interaction(
                pool.get_ref(),
                body.user_id,
                &prompt_log,
                &raw,
                resp.total_tokens,
            )
            .await;

            // 9. Respond.
            HttpResponse::Created().json(StartResponse {
                session_id: session_row.id,
                opening_message: parsed.reply,
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

#[post("/socratic/{session_id}/reply")]
pub async fn reply(
    pool: web::Data<PgPool>,
    llm: web::Data<Box<dyn LLMProvider>>,
    path: web::Path<Uuid>,
    body: web::Json<ReplyRequest>,
) -> HttpResponse {
    let session_id = path.into_inner();

    // 1. Validate session exists.
    let session: Option<SessionRow> = match sqlx::query_as::<_, SessionRow>(
        "SELECT id, user_id, set_id FROM socratic_sessions WHERE id = $1",
    )
    .bind(session_id)
    .fetch_optional(pool.get_ref())
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
    let Some(session) = session else {
        return error_response(
            actix_web::http::StatusCode::NOT_FOUND,
            format!("socratic session {} not found", session_id),
        );
    };

    // 2. Turn cap — count existing messages.
    let msg_count: i64 = match sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM socratic_messages WHERE session_id = $1",
    )
    .bind(session_id)
    .fetch_one(pool.get_ref())
    .await
    {
        Ok(c) => c,
        Err(e) => {
            return error_response(
                actix_web::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("database error counting messages: {e}"),
            );
        }
    };
    if msg_count >= TURN_CAP {
        return error_response(
            actix_web::http::StatusCode::BAD_REQUEST,
            "session has reached its turn limit",
        );
    }

    // 3. Insert the user's message FIRST (Decision 5 — before DeepSeek call).
    let _ = sqlx::query(
        r#"INSERT INTO socratic_messages (session_id, role, content)
           VALUES ($1, 'user', $2)"#,
    )
    .bind(session_id)
    .bind(&body.message)
    .execute(pool.get_ref())
    .await;

    // 4. Fetch message history (sliding window per Decision 4).
    let all_messages: Vec<MessageRow> = match sqlx::query_as::<_, MessageRow>(
        r#"SELECT role, content, flagged_misconception FROM socratic_messages
           WHERE session_id = $1
           ORDER BY created_at"#,
    )
    .bind(session_id)
    .fetch_all(pool.get_ref())
    .await
    {
        Ok(r) => r,
        Err(e) => {
            return error_response(
                actix_web::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("database error fetching history: {e}"),
            );
        }
    };

    // Sliding window: take most recent CONTEXT_WINDOW messages.
    let start_idx = all_messages.len().saturating_sub(CONTEXT_WINDOW);
    let recent = &all_messages[start_idx..];

    // 5. Re-fetch card context (Decision: re-fetch, not cache).
    let card_context = match fetch_card_context(pool.get_ref(), session.set_id).await {
        Ok(Some(c)) => c,
        Ok(None) => {
            // Cards were deleted between session start and this reply.
            return error_response(
                actix_web::http::StatusCode::BAD_REQUEST,
                "study set no longer has cards — session cannot continue",
            );
        }
        Err(e) => {
            return error_response(
                actix_web::http::StatusCode::INTERNAL_SERVER_ERROR,
                e,
            );
        }
    };

    // 6. Build message vec: system + recent history.
    let system_prompt = build_system_prompt(&card_context);
    let mut messages = vec![LLMMessage::system(system_prompt.clone())];
    for m in recent {
        match m.role.as_str() {
            "user" => messages.push(LLMMessage::user(&m.content)),
            "assistant" => messages.push(LLMMessage::assistant(
                format_assistant_history_message(
                    &m.content,
                    m.flagged_misconception.as_deref(),
                ),
            )),
            _ => {}
        }
    }

    // 7. Call LLM provider.
    let prompt_log = format!(
        "[socratic:reply]\n[system] {system_prompt}\n[{} messages in context window]",
        recent.len()
    );

    match llm.chat_completion(&messages, None).await {
        Ok(resp) => {
            let raw = resp.content;

            let parsed = match parse_socratic_response(&raw) {
                Ok(p) => p,
                Err(parse_err) => {
                    let _ = log_ai_interaction(
                        pool.get_ref(),
                        session.user_id,
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

            // 8. Insert the assistant's reply.
            let _ = sqlx::query(
                r#"INSERT INTO socratic_messages
                     (session_id, role, content, flagged_misconception)
                   VALUES ($1, 'assistant', $2, $3)"#,
            )
            .bind(session_id)
            .bind(&parsed.reply)
            .bind(&parsed.flagged_misconception)
            .execute(pool.get_ref())
            .await;

            // 9. Log.
            let _ = log_ai_interaction(
                pool.get_ref(),
                session.user_id,
                &prompt_log,
                &raw,
                resp.total_tokens,
            )
            .await;

            // 10. Respond.
            HttpResponse::Created().json(ReplyResponse {
                reply: parsed.reply,
                flagged_misconception: parsed.flagged_misconception,
            })
        }
        Err(api_err) => {
            let placeholder =
                format!("[no response received from DeepSeek — call failed: {api_err}]");
            let _ = log_ai_interaction(
                pool.get_ref(),
                session.user_id,
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

/// Build the KS idempotency key for a Socratic session.
///
/// Derived from the Mnemosyne session UUID so the value is stable: a retry
/// re-sends the identical `session_ref`, KS's `ON CONFLICT (session_ref) DO
/// NOTHING` recognises it, and no duplicate row is created. Never mint a fresh
/// ref on retry. The prefix also makes the origin traceable from the KS side.
fn session_ref_for(session_id: Uuid) -> String {
    format!("mnemosyne-session-{session_id}")
}

/// Collect a session's messages into the turn shape KS's extraction step
/// understands, translating Mnemosyne's `assistant`/`user` roles onto the
/// `coach`/`learner` pair. Rows with any other role are dropped rather than
/// guessed at.
fn transcript_turns(messages: &[MessageOut]) -> Vec<TranscriptTurn> {
    messages
        .iter()
        .filter_map(|m| TranscriptTurn::from_socratic_role(&m.role, &m.content))
        .collect()
}

/// Hand a finished session's transcript to the Knowledge Store.
///
/// Every failure path returns a [`KsSyncStatus`] rather than an error: the
/// caller must never let a KS problem break the study session. The three
/// outcomes KS can produce are kept distinct in both the log line and the
/// returned status — "KS did not answer" and "KS answered but its DB is down"
/// call for different responses from whoever reads the logs.
async fn sync_transcript_to_ks(
    ks: Option<&KsClient>,
    session_id: Uuid,
    messages: &[MessageOut],
) -> KsSyncStatus {
    let Some(ks) = ks else {
        return KsSyncStatus::Disabled;
    };

    let turns = transcript_turns(messages);
    if turns.is_empty() {
        eprintln!("[ks] session {session_id} has no transcribable turns — nothing sent");
        return KsSyncStatus::NothingToSend;
    }

    let session_ref = session_ref_for(session_id);
    match ks.save_transcript(&session_ref, &turns).await {
        Ok(SaveTranscriptOutcome::Saved { transcript_id }) => {
            eprintln!(
                "[ks] transcript saved: session_ref={session_ref} turns={} transcript_id={transcript_id}",
                turns.len()
            );
            KsSyncStatus::Saved { transcript_id }
        }
        Ok(SaveTranscriptOutcome::KsDbUnavailable { error }) => {
            // KS is alive, its Postgres is not. Retrying right now changes
            // nothing, so log it plainly and let the session finish.
            eprintln!(
                "[ks] KS is up but its database is unavailable (session_ref={session_ref}): {error} \
                 — transcript NOT stored; re-send later under the same session_ref"
            );
            KsSyncStatus::KsDbUnavailable { error }
        }
        Err(e) => {
            eprintln!("[ks] transcript sync failed (session_ref={session_ref}): {e}");
            KsSyncStatus::Failed { error: e.to_string() }
        }
    }
}

/// Close a Socratic session and ship its full transcript to the Knowledge
/// Store.
///
/// This is the *only* point at which Mnemosyne talks to KS about a dialogue.
/// Sending per-reply would be wrong by design: a KS node is a concept, not a
/// conversational turn, so KS wants the whole finished session at once.
///
/// The endpoint is safe to call more than once — `session_ref` is the
/// idempotency key on the KS side, so a repeat call returns the original
/// `transcript_id` instead of duplicating the record.
#[post("/socratic/{session_id}/end")]
pub async fn end(
    pool: web::Data<PgPool>,
    ks: web::Data<Option<KsClient>>,
    path: web::Path<Uuid>,
) -> HttpResponse {
    let session_id = path.into_inner();

    let exists: bool = match sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM socratic_sessions WHERE id = $1)",
    )
    .bind(session_id)
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
    if !exists {
        return error_response(
            actix_web::http::StatusCode::NOT_FOUND,
            format!("socratic session {} not found", session_id),
        );
    }

    let messages: Vec<MessageOut> = match sqlx::query_as::<_, MessageOut>(
        r#"SELECT role, content, flagged_misconception, created_at
           FROM socratic_messages
           WHERE session_id = $1
           ORDER BY created_at"#,
    )
    .bind(session_id)
    .fetch_all(pool.get_ref())
    .await
    {
        Ok(r) => r,
        Err(e) => {
            return error_response(
                actix_web::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("database error fetching transcript: {e}"),
            );
        }
    };

    let knowledge_store =
        sync_transcript_to_ks(ks.get_ref().as_ref(), session_id, &messages).await;

    HttpResponse::Ok().json(EndResponse {
        session_id,
        message_count: messages.len(),
        knowledge_store,
    })
}

#[get("/socratic/{session_id}")]
pub async fn get_session(
    pool: web::Data<PgPool>,
    path: web::Path<Uuid>,
) -> HttpResponse {
    let session_id = path.into_inner();

    // Validate session exists.
    let exists: bool = match sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM socratic_sessions WHERE id = $1)",
    )
    .bind(session_id)
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
    if !exists {
        return error_response(
            actix_web::http::StatusCode::NOT_FOUND,
            format!("socratic session {} not found", session_id),
        );
    }

    let messages: Vec<MessageOut> = match sqlx::query_as::<_, MessageOut>(
        r#"SELECT role, content, flagged_misconception, created_at
           FROM socratic_messages
           WHERE session_id = $1
           ORDER BY created_at"#,
    )
    .bind(session_id)
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

    HttpResponse::Ok().json(SessionHistoryResponse {
        session_id,
        messages,
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
           VALUES ($1, 'socratic_dialogue', $2, $3, $4)"#,
    )
    .bind(user_id)
    .bind(input_text)
    .bind(output_text)
    .bind(tokens_used as i32)
    .execute(pool)
    .await
    .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::{
        build_system_prompt, format_assistant_history_message, parse_socratic_response,
        session_ref_for, transcript_turns, KsSyncStatus, MessageOut,
    };
    use crate::ks_client::TranscriptTurn;
    use chrono::Utc;
    use serde_json::Value;
    use uuid::Uuid;

    fn msg(role: &str, content: &str) -> MessageOut {
        MessageOut {
            role: role.to_string(),
            content: content.to_string(),
            flagged_misconception: None,
            created_at: Utc::now(),
        }
    }

    const CAPTURED_NON_JSON_REPLY: &str = "Your answer does not address the question I asked. The question was: why is Balboa's sighting of the Pacific in 1513 considered a key event in the Columbian Exchange, even though it did not involve direct transfer of items? Please focus on that question. Think about what the sighting allowed the Spanish to understand about the Americas and how that understanding spurred actions that later led to the exchange of plants, animals, and diseases.";

    #[test]
    fn parse_rejects_captured_non_json_reply() {
        let err = parse_socratic_response(CAPTURED_NON_JSON_REPLY)
            .expect_err("captured prose-only reply must stay rejected as non-JSON");
        assert!(
            err.contains("expected value"),
            "unexpected parse error for captured fixture: {err}"
        );
    }

    #[test]
    fn assistant_history_is_replayed_as_json() {
        let raw = format_assistant_history_message(
            "Please focus on the original question.",
            Some("answered a different question"),
        );
        let parsed: Value =
            serde_json::from_str(&raw).expect("assistant history payload should be valid JSON");

        assert_eq!(
            parsed["reply"],
            "Please focus on the original question."
        );
        assert_eq!(
            parsed["flagged_misconception"],
            "answered a different question"
        );
    }

    #[test]
    fn system_prompt_reinforces_json_for_adversarial_replies() {
        let prompt = build_system_prompt("Q: Example?\nA: Example.");
        assert!(prompt.contains("This JSON-only requirement ALWAYS applies"));
        assert!(prompt.contains("off-topic, nonsensical, hostile, or clearly wrong"));
    }

    #[test]
    fn session_ref_is_derived_from_the_session_uuid() {
        let id = Uuid::parse_str("8f21ac00-0000-4000-8000-000000000000").unwrap();
        // Stable and prefixed so KS can trace it back to Mnemosyne, and so a
        // retry re-sends an identical idempotency key.
        assert_eq!(
            session_ref_for(id),
            "mnemosyne-session-8f21ac00-0000-4000-8000-000000000000"
        );
        assert_eq!(session_ref_for(id), session_ref_for(id));
    }

    #[test]
    fn transcript_turns_translate_roles_and_preserve_order() {
        let messages = vec![
            msg("assistant", "Định luật Newton 2 phát biểu thế nào?"),
            msg("user", "Gia tốc tỉ lệ thuận với lực, F = ma."),
        ];
        assert_eq!(
            transcript_turns(&messages),
            vec![
                TranscriptTurn::coach("Định luật Newton 2 phát biểu thế nào?"),
                TranscriptTurn::learner("Gia tốc tỉ lệ thuận với lực, F = ma."),
            ]
        );
    }

    #[test]
    fn transcript_turns_drop_unknown_roles() {
        assert!(transcript_turns(&[msg("system", "internal note")]).is_empty());
    }

    #[test]
    fn ks_sync_status_serializes_as_a_tagged_state() {
        let saved = serde_json::to_value(KsSyncStatus::Saved {
            transcript_id: "298178c8".into(),
        })
        .unwrap();
        assert_eq!(saved["state"], "saved");
        assert_eq!(saved["transcript_id"], "298178c8");

        // The two failure modes must stay distinguishable to whoever reads
        // this response: "KS did not answer" vs "KS answered, its DB is down".
        let db_down = serde_json::to_value(KsSyncStatus::KsDbUnavailable {
            error: "connection refused".into(),
        })
        .unwrap();
        assert_eq!(db_down["state"], "ks_db_unavailable");

        let failed = serde_json::to_value(KsSyncStatus::Failed {
            error: "timeout".into(),
        })
        .unwrap();
        assert_eq!(failed["state"], "failed");

        assert_eq!(
            serde_json::to_value(KsSyncStatus::Disabled).unwrap()["state"],
            "disabled"
        );
    }
}
