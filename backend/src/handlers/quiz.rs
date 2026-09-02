//! Multiple-choice quiz endpoints.
//!
//! - `POST /quiz/generate`              — generate questions for a study set
//! - `POST /quiz/{question_id}/attempt` — submit an answer, get graded
//! - `GET  /quiz/{set_id}`              — list a set's questions for answering
//!
//! Quiz is the one methodology here whose grading needs no model. Socratic is
//! a dialogue and Feynman is free text scored by the LLM; a quiz answer is an
//! index, so checking it is a comparison against the stored `correct_index`.
//! Grading therefore costs no tokens, cannot fail on a network hiccup, and
//! returns the same verdict every time.
//!
//! ## Where the questions come from
//!
//! Exactly one of two sources per request, never a mix:
//!
//! * `topic` — free text the user supplies, generated the same way
//!   [`crate::handlers::generate`] builds flashcards.
//! * `knowledge_store` — concept nodes the learner has already studied,
//!   fetched from the Knowledge Store over HTTP. Each generated question keeps
//!   the id of the node it came from.
//!
//! ## Not leaking the answer
//!
//! `correct_index` must not reach the client until the learner has answered —
//! otherwise it is visible in DevTools before they pick. That is enforced
//! structurally rather than by remembering to strip a field. The generation
//! and listing endpoints read rows into [`QuizQuestionRow`], whose SQL does
//! not select `correct_index` at all, and serialize [`QuizQuestionOut`], which
//! has no such field. The answer is read in exactly one place, [`GradingRow`]
//! on the attempt path, and returned in exactly one response,
//! [`AttemptResponse`] — after the learner has committed.

use actix_web::{get, post, web, HttpResponse};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::types::Json;
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

use crate::ks_client::{KsClient, KsNode};
use crate::llm_provider::{LLMMessage, LLMProvider};
use super::{describe_llm_failure, error_response};

/// Upper bound on questions per request, to keep token cost predictable.
const MAX_QUESTION_COUNT: u32 = 20;

/// Cap on free-text topic length. Mirrors `generate.rs`'s source-text cap.
const MAX_TOPIC_CHARS: usize = 8000;

/// A "multiple choice" question with one option is not a choice; more than six
/// is unreadable on a phone. Both ends are validated against LLM output.
const MIN_CHOICES: usize = 2;
const MAX_CHOICES: usize = 6;

/// Cap on how many Knowledge Store nodes are fed into one prompt, and on the
/// total characters they contribute. Keeps the prompt bounded no matter how
/// much the learner has studied.
const MAX_NODES_PER_PROMPT: usize = 30;
const MAX_NODE_CONTEXT_CHARS: usize = 6000;

/// Accepted values of `quiz_questions.source`. Kept in sync with the CHECK
/// constraint in the schema.
const SOURCE_TOPIC: &str = "topic";
const SOURCE_KNOWLEDGE_STORE: &str = "knowledge_store";

// ---------------------------------------------------------------------------
// Request / response types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct GenerateQuizRequest {
    pub study_set_id: Uuid,
    /// `"topic"` or `"knowledge_store"`.
    pub source: String,
    /// Required when `source == "topic"`.
    pub topic: Option<String>,
    /// Optional subject narrowing when `source == "knowledge_store"`.
    pub subject_filter: Option<String>,
    pub count: u32,
}

#[derive(Debug, Deserialize)]
pub struct AttemptRequest {
    pub user_id: Uuid,
    pub selected_index: i32,
}

/// A question as the client is allowed to see it — deliberately **without**
/// `correct_index`. This is the only serializable question type in the module.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct QuizQuestionOut {
    pub id: Uuid,
    pub set_id: Uuid,
    pub question: String,
    pub choices: Vec<String>,
    pub source: String,
    pub source_node_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
pub struct GenerateQuizResponse {
    pub questions: Vec<QuizQuestionOut>,
    pub tokens_used: u32,
}

#[derive(Debug, Serialize)]
pub struct AttemptResponse {
    pub is_correct: bool,
    /// Revealed only here — after the learner has committed to an answer.
    pub correct_index: i32,
}

#[derive(Debug, Serialize)]
pub struct ListQuestionsResponse {
    pub set_id: Uuid,
    pub questions: Vec<QuizQuestionOut>,
    pub count: usize,
}

// ---------------------------------------------------------------------------
// Internal DB row types
// ---------------------------------------------------------------------------

/// A question row as read back for display. It deliberately does **not**
/// include `correct_index` — the answer is never even fetched on the paths
/// that build a response, so it cannot leak through them by oversight. The
/// grading path uses [`GradingRow`] instead, which is the only place the
/// answer is read.
#[derive(Debug, FromRow)]
struct QuizQuestionRow {
    id: Uuid,
    set_id: Uuid,
    question: String,
    choices: Json<Vec<String>>,
    source: String,
    source_node_id: Option<Uuid>,
    created_at: DateTime<Utc>,
}

impl From<QuizQuestionRow> for QuizQuestionOut {
    fn from(row: QuizQuestionRow) -> Self {
        Self {
            id: row.id,
            set_id: row.set_id,
            question: row.question,
            choices: row.choices.0,
            source: row.source,
            source_node_id: row.source_node_id,
            created_at: row.created_at,
        }
    }
}

/// Just the fields grading needs, so the attempt path does not pull whole rows.
#[derive(Debug, FromRow)]
struct GradingRow {
    correct_index: i32,
    choices: Json<Vec<String>>,
}

#[derive(Debug, FromRow)]
struct StudySetOwnerRow {
    user_id: Uuid,
}

// ---------------------------------------------------------------------------
// LLM output parsing
// ---------------------------------------------------------------------------

/// One question as the model is asked to emit it.
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
struct GeneratedQuestion {
    question: String,
    choices: Vec<String>,
    correct_index: i32,
    /// Index into the node list given in the prompt. Present only in
    /// knowledge-store mode; ignored otherwise.
    #[serde(default)]
    source_node_index: Option<usize>,
}

/// Defensive parse: try the raw text, then strip a ```json fence. Same shape
/// of leniency as `generate.rs` and `socratic.rs`, for the same reason — the
/// model intermittently wraps JSON in a fence despite instructions.
fn parse_generated_questions(raw: &str) -> Result<Vec<GeneratedQuestion>, String> {
    if let Ok(v) = serde_json::from_str::<Vec<GeneratedQuestion>>(raw) {
        return Ok(v);
    }
    let trimmed = raw.trim();
    let stripped: &str = if trimmed.starts_with("```") {
        let after_open = trimmed
            .strip_prefix("```json")
            .or_else(|| trimmed.strip_prefix("```"))
            .unwrap_or(trimmed);
        after_open.strip_suffix("```").unwrap_or(after_open).trim()
    } else {
        trimmed
    };
    serde_json::from_str::<Vec<GeneratedQuestion>>(stripped)
        .map_err(|e| format!("{e} (after stripping fences)"))
}

/// Reject the whole batch if any question is malformed, rather than inserting
/// the good ones — the same atomicity choice `generate.rs` makes. A quiz with
/// silently missing questions is harder to notice than one that failed loudly.
///
/// `node_count` bounds `source_node_index`; pass 0 in topic mode, where the
/// field is not used.
fn validate_generated(
    questions: &[GeneratedQuestion],
    node_count: usize,
) -> Result<(), String> {
    if questions.is_empty() {
        return Err("model returned an empty array; nothing to insert".to_string());
    }
    for (i, q) in questions.iter().enumerate() {
        if q.question.trim().is_empty() {
            return Err(format!("question at index {i} is empty"));
        }
        if q.choices.len() < MIN_CHOICES || q.choices.len() > MAX_CHOICES {
            return Err(format!(
                "question at index {i} has {} choices; must be between {MIN_CHOICES} and {MAX_CHOICES}",
                q.choices.len()
            ));
        }
        if let Some(blank) = q.choices.iter().position(|c| c.trim().is_empty()) {
            return Err(format!("question at index {i} has an empty choice at position {blank}"));
        }
        if q.correct_index < 0 || q.correct_index as usize >= q.choices.len() {
            return Err(format!(
                "question at index {i} has correct_index {} outside its {} choices",
                q.correct_index,
                q.choices.len()
            ));
        }
        if node_count > 0 {
            match q.source_node_index {
                Some(idx) if idx < node_count => {}
                Some(idx) => {
                    return Err(format!(
                        "question at index {i} cites source_node_index {idx}, but only {node_count} nodes were provided"
                    ));
                }
                None => {
                    return Err(format!(
                        "question at index {i} is missing source_node_index, required in knowledge_store mode"
                    ));
                }
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Grading
// ---------------------------------------------------------------------------

/// Decide whether an answer is right.
///
/// This is the whole of quiz grading: an index comparison. It is a named
/// function rather than an inline `==` so the rule is stated in one place and
/// can be tested without a database or a model — and so that any future
/// temptation to "improve" grading with an LLM has to come through here and
/// be argued for explicitly.
fn is_answer_correct(selected_index: i32, correct_index: i32) -> bool {
    selected_index == correct_index
}

// ---------------------------------------------------------------------------
// Prompt construction
// ---------------------------------------------------------------------------

/// Shared tail describing the required JSON shape. Kept in one place so the
/// two prompt builders cannot drift apart on the part that must match
/// [`GeneratedQuestion`].
fn output_format_rules(n: u32, with_node_index: bool) -> String {
    let node_field = if with_node_index {
        ", \"source_node_index\": integer"
    } else {
        ""
    };
    let node_rule = if with_node_index {
        "\n`source_node_index` MUST be the 0-based number of the concept the \
         question was written from, exactly as numbered in the list above."
    } else {
        ""
    };
    format!(
        "Respond with ONLY valid JSON: an array of exactly {n} objects, each \
         shaped {{\"question\": string, \"choices\": array of strings, \
         \"correct_index\": integer{node_field}}}.\n\
         Each question MUST have exactly 4 choices. Exactly ONE choice is \
         correct, and `correct_index` is its 0-based position in `choices`. \
         The wrong choices must be plausible — a learner who does not know the \
         material should not be able to eliminate them by length, phrasing, or \
         obvious absurdity. Vary which position holds the correct answer.\
         {node_rule}\n\
         No prose, no markdown fences, no commentary outside the JSON."
    )
}

fn build_topic_prompt(n: u32, topic: &str) -> (String, String) {
    let system = format!(
        "You are a quiz author. Write exactly {n} multiple-choice questions \
         testing understanding of the material the user provides.\n\n{rules}",
        rules = output_format_rules(n, false)
    );
    let user = format!("Material:\n\n{topic}\n\nWrite {n} multiple-choice questions.");
    (system, user)
}

/// Render the node list and the prompt that quizzes on it.
///
/// All nodes go into a single call with an explicit numbering, and the model
/// reports which number each question came from. The alternative — one call
/// per node — would attribute questions trivially but multiply the cost by the
/// number of concepts studied.
fn build_knowledge_store_prompt(n: u32, nodes: &[KsNode]) -> (String, String) {
    let mut listing = String::new();
    for (i, node) in nodes.iter().enumerate() {
        let entry = format!("[{i}] {} ({})\n{}\n\n", node.title, node.subject, node.summary);
        if listing.len() + entry.len() > MAX_NODE_CONTEXT_CHARS {
            break;
        }
        listing.push_str(&entry);
    }
    let system = format!(
        "You are a quiz author. Write exactly {n} multiple-choice questions \
         testing the concepts the user has already studied. Base every question \
         on one of the numbered concepts; do not invent material outside \
         them.\n\n{rules}",
        rules = output_format_rules(n, true)
    );
    let user = format!("Concepts studied:\n\n{listing}Write {n} multiple-choice questions.");
    (system, user)
}

// ---------------------------------------------------------------------------
// Endpoints
// ---------------------------------------------------------------------------

#[post("/quiz/generate")]
pub async fn generate_quiz(
    pool: web::Data<PgPool>,
    llm: web::Data<Box<dyn LLMProvider>>,
    ks: web::Data<Option<KsClient>>,
    body: web::Json<GenerateQuizRequest>,
) -> HttpResponse {
    // 1. Validate count.
    if body.count == 0 {
        return error_response(
            actix_web::http::StatusCode::BAD_REQUEST,
            "count must be >= 1",
        );
    }
    let count = body.count.min(MAX_QUESTION_COUNT);

    // 2. Validate source. The two sources are never mixed in one request.
    let source = body.source.trim().to_lowercase();
    if source != SOURCE_TOPIC && source != SOURCE_KNOWLEDGE_STORE {
        return error_response(
            actix_web::http::StatusCode::BAD_REQUEST,
            format!(
                "invalid source '{}': must be '{SOURCE_TOPIC}' or '{SOURCE_KNOWLEDGE_STORE}'",
                body.source
            ),
        );
    }

    // 3. Validate the study set exists and get its owner, needed for the
    //    ai_interactions row (user_id is NOT NULL with an FK).
    let owner: Option<StudySetOwnerRow> =
        match sqlx::query_as::<_, StudySetOwnerRow>("SELECT user_id FROM study_sets WHERE id = $1")
            .bind(body.study_set_id)
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
            format!("study set {} not found", body.study_set_id),
        );
    };

    // 4. Gather source material and build the prompt.
    let (system_prompt, user_prompt, nodes) = if source == SOURCE_TOPIC {
        let topic = body.topic.as_deref().unwrap_or("").trim().to_string();
        if topic.is_empty() {
            return error_response(
                actix_web::http::StatusCode::BAD_REQUEST,
                "topic is required when source is 'topic'",
            );
        }
        if topic.chars().count() > MAX_TOPIC_CHARS {
            return error_response(
                actix_web::http::StatusCode::BAD_REQUEST,
                format!("topic exceeds {MAX_TOPIC_CHARS} character limit"),
            );
        }
        let (s, u) = build_topic_prompt(count, &topic);
        (s, u, Vec::new())
    } else {
        // Knowledge-store mode. A missing KS client is a configuration state,
        // not a client error: report it as unavailable rather than pretending
        // the learner has studied nothing.
        let Some(ks) = ks.get_ref().as_ref() else {
            return error_response(
                actix_web::http::StatusCode::SERVICE_UNAVAILABLE,
                "Knowledge Store is not configured (KS_HTTP_TOKEN unset); \
                 generate with source 'topic' instead",
            );
        };
        let nodes = match ks.get_nodes(body.subject_filter.as_deref()).await {
            Ok(n) => n,
            Err(e) => {
                eprintln!("[ks] GET /nodes failed during quiz generation: {e}");
                return error_response(
                    actix_web::http::StatusCode::BAD_GATEWAY,
                    format!("could not read concepts from the Knowledge Store: {e}"),
                );
            }
        };
        if nodes.is_empty() {
            return error_response(
                actix_web::http::StatusCode::BAD_REQUEST,
                match body.subject_filter.as_deref() {
                    Some(subject) => format!(
                        "the Knowledge Store has no concepts for subject '{subject}' yet — \
                         nothing to quiz on"
                    ),
                    None => "the Knowledge Store has no concepts yet — nothing to quiz on"
                        .to_string(),
                },
            );
        }
        let nodes: Vec<KsNode> = nodes.into_iter().take(MAX_NODES_PER_PROMPT).collect();
        let (s, u) = build_knowledge_store_prompt(count, &nodes);
        (s, u, nodes)
    };

    let messages = vec![
        LLMMessage::system(system_prompt.clone()),
        LLMMessage::user(user_prompt.clone()),
    ];
    let prompt_log =
        format!("[source: {source}]\n[system] {system_prompt}\n[user] {user_prompt}");

    // 5. Call the LLM. This is the only model call in the whole quiz feature —
    //    grading below never touches it.
    let resp = match llm.chat_completion(&messages, None).await {
        Ok(r) => r,
        Err(api_err) => {
            let (placeholder, message) = describe_llm_failure(&api_err);
            let _ =
                log_ai_interaction(pool.get_ref(), owner.user_id, &prompt_log, &placeholder, 0)
                    .await;
            return error_response(actix_web::http::StatusCode::BAD_GATEWAY, message);
        }
    };
    let raw = resp.content;

    let generated = match parse_generated_questions(&raw) {
        Ok(g) => g,
        Err(parse_err) => {
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

    if let Err(validation_err) = validate_generated(&generated, nodes.len()) {
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
            format!("DeepSeek returned a malformed quiz. Batch rejected; nothing inserted: {validation_err}"),
        );
    }

    // 6. Insert. Stop on the first failure and say how far we got, matching
    //    generate.rs.
    let mut created: Vec<QuizQuestionOut> = Vec::with_capacity(generated.len());
    for q in &generated {
        let source_node_id: Option<Uuid> = q
            .source_node_index
            .and_then(|i| nodes.get(i))
            .and_then(|n| Uuid::parse_str(&n.id).ok());

        // The schema requires source_node_id to be present exactly when
        // source='knowledge_store'. If a node id came back unparseable we
        // would violate that constraint, so fail clearly instead.
        if source == SOURCE_KNOWLEDGE_STORE && source_node_id.is_none() {
            return error_response(
                actix_web::http::StatusCode::BAD_GATEWAY,
                "the Knowledge Store returned a node id that is not a UUID; cannot attribute questions",
            );
        }

        match sqlx::query_as::<_, QuizQuestionRow>(
            r#"INSERT INTO quiz_questions
                 (set_id, question, choices, correct_index, source, source_node_id)
               VALUES ($1, $2, $3, $4, $5, $6)
               RETURNING id, set_id, question, choices, source,
                         source_node_id, created_at"#,
        )
        .bind(body.study_set_id)
        .bind(&q.question)
        .bind(Json(&q.choices))
        .bind(q.correct_index)
        .bind(&source)
        .bind(source_node_id)
        .fetch_one(pool.get_ref())
        .await
        {
            Ok(row) => created.push(row.into()),
            Err(e) => {
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
                        "quiz question insertion failed after {} of {} inserted: {e}",
                        created.len(),
                        generated.len()
                    ),
                );
            }
        }
    }

    let _ = log_ai_interaction(
        pool.get_ref(),
        owner.user_id,
        &prompt_log,
        &raw,
        resp.total_tokens,
    )
    .await;

    // `created` is Vec<QuizQuestionOut>, so no answer can leave here.
    HttpResponse::Created().json(GenerateQuizResponse {
        questions: created,
        tokens_used: resp.total_tokens,
    })
}

#[post("/quiz/{question_id}/attempt")]
pub async fn attempt(
    pool: web::Data<PgPool>,
    path: web::Path<Uuid>,
    body: web::Json<AttemptRequest>,
) -> HttpResponse {
    let question_id = path.into_inner();

    // 1. Load only what grading needs.
    let row: Option<GradingRow> = match sqlx::query_as::<_, GradingRow>(
        "SELECT correct_index, choices FROM quiz_questions WHERE id = $1",
    )
    .bind(question_id)
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
    let Some(row) = row else {
        return error_response(
            actix_web::http::StatusCode::NOT_FOUND,
            format!("quiz question {question_id} not found"),
        );
    };

    // 2. The answer must be one of the offered options. An out-of-range index
    //    is a malformed request, not a wrong answer — recording it as "wrong"
    //    would quietly corrupt the learner's score history.
    let choice_count = row.choices.0.len();
    if body.selected_index < 0 || body.selected_index as usize >= choice_count {
        return error_response(
            actix_web::http::StatusCode::BAD_REQUEST,
            format!(
                "selected_index {} is outside the {choice_count} choices for this question",
                body.selected_index
            ),
        );
    }

    // 3. Grade. No LLM: the answer is an index, so this is a comparison.
    let is_correct = is_answer_correct(body.selected_index, row.correct_index);

    // 4. Record the attempt.
    if let Err(e) = sqlx::query(
        r#"INSERT INTO quiz_attempts (question_id, user_id, selected_index, is_correct)
           VALUES ($1, $2, $3, $4)"#,
    )
    .bind(question_id)
    .bind(body.user_id)
    .bind(body.selected_index)
    .bind(is_correct)
    .execute(pool.get_ref())
    .await
    {
        let (status, message) = super::classify_db_error(&e);
        return error_response(status, message);
    }

    // 5. Only now is the answer disclosed.
    HttpResponse::Created().json(AttemptResponse {
        is_correct,
        correct_index: row.correct_index,
    })
}

#[get("/quiz/{set_id}")]
pub async fn list_questions(pool: web::Data<PgPool>, path: web::Path<Uuid>) -> HttpResponse {
    let set_id = path.into_inner();

    let exists: bool = match sqlx::query_scalar::<_, bool>(
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
    if !exists {
        return error_response(
            actix_web::http::StatusCode::NOT_FOUND,
            format!("study set {set_id} not found"),
        );
    }

    let rows: Vec<QuizQuestionRow> = match sqlx::query_as::<_, QuizQuestionRow>(
        r#"SELECT id, set_id, question, choices, source,
                  source_node_id, created_at
           FROM quiz_questions
           WHERE set_id = $1
           ORDER BY created_at"#,
    )
    .bind(set_id)
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

    // Neither the row type nor the output type carries the answer, so this
    // endpoint — the answering screen's — cannot expose one.
    let questions: Vec<QuizQuestionOut> = rows.into_iter().map(Into::into).collect();

    HttpResponse::Ok().json(ListQuestionsResponse {
        set_id,
        count: questions.len(),
        questions,
    })
}

// ---------------------------------------------------------------------------
// Shared helper
// ---------------------------------------------------------------------------

/// Log the generation call for cost tracking. Grading is absent from
/// `ai_interactions` by design — it never calls a model.
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
           VALUES ($1, 'quiz_generation', $2, $3, $4)"#,
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
    use super::*;

    fn q(correct_index: i32, choices: &[&str]) -> GeneratedQuestion {
        GeneratedQuestion {
            question: "Định luật Newton 2 phát biểu thế nào?".to_string(),
            choices: choices.iter().map(|c| c.to_string()).collect(),
            correct_index,
            source_node_index: None,
        }
    }

    const FOUR: [&str; 4] = ["F = ma", "F = mv", "F = m/a", "F = a/m"];

    // -- parsing model output ------------------------------------------------

    #[test]
    fn parses_plain_json_array() {
        let raw = r#"[{"question":"Q?","choices":["a","b","c","d"],"correct_index":2}]"#;
        let parsed = parse_generated_questions(raw).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].correct_index, 2);
        assert_eq!(parsed[0].choices.len(), 4);
    }

    #[test]
    fn parses_json_wrapped_in_a_markdown_fence() {
        // The model does this intermittently despite being told not to.
        let raw = "```json\n[{\"question\":\"Q?\",\"choices\":[\"a\",\"b\"],\"correct_index\":0}]\n```";
        assert_eq!(parse_generated_questions(raw).unwrap().len(), 1);
    }

    #[test]
    fn parses_source_node_index_when_present() {
        let raw = r#"[{"question":"Q?","choices":["a","b"],"correct_index":1,"source_node_index":3}]"#;
        assert_eq!(parse_generated_questions(raw).unwrap()[0].source_node_index, Some(3));
    }

    #[test]
    fn prose_reply_is_rejected_as_unparseable() {
        assert!(parse_generated_questions("Here are your questions!").is_err());
    }

    // -- validation ----------------------------------------------------------

    #[test]
    fn accepts_a_well_formed_batch() {
        assert!(validate_generated(&[q(0, &FOUR), q(3, &FOUR)], 0).is_ok());
    }

    #[test]
    fn rejects_empty_batch() {
        assert!(validate_generated(&[], 0).is_err());
    }

    #[test]
    fn rejects_correct_index_past_the_end_of_choices() {
        // Would otherwise store a permanently unanswerable question.
        let err = validate_generated(&[q(4, &FOUR)], 0).unwrap_err();
        assert!(err.contains("correct_index"), "unexpected error: {err}");
    }

    #[test]
    fn rejects_negative_correct_index() {
        assert!(validate_generated(&[q(-1, &FOUR)], 0).is_err());
    }

    #[test]
    fn rejects_single_choice_question() {
        // One option is not a multiple choice.
        assert!(validate_generated(&[q(0, &["only"])], 0).is_err());
    }

    #[test]
    fn rejects_too_many_choices() {
        let many = ["a", "b", "c", "d", "e", "f", "g"];
        assert!(validate_generated(&[q(0, &many)], 0).is_err());
    }

    #[test]
    fn rejects_blank_choice() {
        assert!(validate_generated(&[q(0, &["F = ma", "   ", "c", "d"])], 0).is_err());
    }

    #[test]
    fn rejects_blank_question_text() {
        let mut bad = q(0, &FOUR);
        bad.question = "   ".to_string();
        assert!(validate_generated(&[bad], 0).is_err());
    }

    #[test]
    fn one_bad_question_rejects_the_whole_batch() {
        // Atomic, like generate.rs: a quiz silently missing questions is
        // harder to notice than one that failed loudly.
        assert!(validate_generated(&[q(0, &FOUR), q(9, &FOUR), q(1, &FOUR)], 0).is_err());
    }

    #[test]
    fn knowledge_store_mode_requires_a_node_citation() {
        // node_count > 0 means knowledge-store mode.
        let err = validate_generated(&[q(0, &FOUR)], 3).unwrap_err();
        assert!(err.contains("source_node_index"), "unexpected error: {err}");
    }

    #[test]
    fn knowledge_store_mode_rejects_out_of_range_node_citation() {
        let mut cited = q(0, &FOUR);
        cited.source_node_index = Some(5);
        assert!(validate_generated(&[cited], 3).is_err());
    }

    #[test]
    fn knowledge_store_mode_accepts_a_valid_node_citation() {
        let mut cited = q(0, &FOUR);
        cited.source_node_index = Some(2);
        assert!(validate_generated(&[cited], 3).is_ok());
    }

    // -- grading -------------------------------------------------------------
    //
    // Grading is a comparison, never a model call. These pin that behaviour.

    #[test]
    fn grading_accepts_only_the_matching_index() {
        assert!(is_answer_correct(2, 2));
        assert!(!is_answer_correct(0, 2));
        assert!(!is_answer_correct(3, 2));
    }

    #[test]
    fn grading_is_exact_across_every_position() {
        // Every option in a 4-choice question: exactly one selection grades
        // correct, and it is the one matching correct_index.
        for correct in 0..4i32 {
            let correct_count =
                (0..4i32).filter(|s| is_answer_correct(*s, correct)).count();
            assert_eq!(correct_count, 1, "correct_index {correct}");
            assert!(is_answer_correct(correct, correct));
        }
    }

    #[test]
    fn grading_does_not_treat_index_zero_specially() {
        // Guards against a default-value bug where an unanswered or
        // zero-initialised selection would score as right.
        assert!(is_answer_correct(0, 0));
        assert!(!is_answer_correct(0, 1));
    }

    // -- the answer must not leak -------------------------------------------

    fn sample_out() -> QuizQuestionOut {
        QuizQuestionOut {
            id: Uuid::nil(),
            set_id: Uuid::nil(),
            question: "Định luật Newton 2 phát biểu thế nào?".to_string(),
            choices: FOUR.iter().map(|c| c.to_string()).collect(),
            source: SOURCE_TOPIC.to_string(),
            source_node_id: None,
            created_at: Utc::now(),
        }
    }

    #[test]
    fn serialized_question_never_contains_the_answer() {
        let json = serde_json::to_value(sample_out()).unwrap();
        assert!(
            json.get("correct_index").is_none(),
            "correct_index must not appear in a question sent to the client: {json}"
        );
        // The fields the answering screen does need are present.
        assert!(json.get("question").is_some());
        assert_eq!(json["choices"].as_array().unwrap().len(), 4);
    }

    #[test]
    fn generate_response_never_contains_the_answer() {
        let body = serde_json::to_string(&GenerateQuizResponse {
            questions: vec![sample_out()],
            tokens_used: 123,
        })
        .unwrap();
        assert!(
            !body.contains("correct_index"),
            "POST /quiz/generate leaked the answer: {body}"
        );
    }

    #[test]
    fn list_response_never_contains_the_answer() {
        let body = serde_json::to_string(&ListQuestionsResponse {
            set_id: Uuid::nil(),
            questions: vec![sample_out()],
            count: 1,
        })
        .unwrap();
        assert!(
            !body.contains("correct_index"),
            "GET /quiz/{{set_id}} leaked the answer: {body}"
        );
    }

    #[test]
    fn attempt_response_is_the_only_place_the_answer_appears() {
        let json = serde_json::to_value(AttemptResponse {
            is_correct: false,
            correct_index: 2,
        })
        .unwrap();
        assert_eq!(json["is_correct"], false);
        assert_eq!(json["correct_index"], 2);
    }

    // -- prompts -------------------------------------------------------------

    #[test]
    fn topic_prompt_states_the_json_contract() {
        let (system, user) = build_topic_prompt(3, "Định luật Newton");
        assert!(system.contains("exactly 3 objects"));
        assert!(system.contains("correct_index"));
        assert!(!system.contains("source_node_index"));
        assert!(user.contains("Định luật Newton"));
    }

    #[test]
    fn knowledge_store_prompt_numbers_nodes_and_demands_attribution() {
        let nodes = vec![
            KsNode {
                id: "11111111-1111-4111-8111-111111111111".into(),
                title: "Quán tính".into(),
                subject: "Vật lý".into(),
                summary: "Xu hướng giữ nguyên trạng thái chuyển động.".into(),
            },
            KsNode {
                id: "22222222-2222-4222-8222-222222222222".into(),
                title: "Lực tổng hợp".into(),
                subject: "Vật lý".into(),
                summary: "Hợp lực của mọi lực tác dụng lên vật.".into(),
            },
        ];
        let (system, user) = build_knowledge_store_prompt(2, &nodes);
        assert!(system.contains("source_node_index"));
        assert!(user.contains("[0] Quán tính"));
        assert!(user.contains("[1] Lực tổng hợp"));
    }
}
