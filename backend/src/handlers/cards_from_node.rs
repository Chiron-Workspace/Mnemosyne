//! `POST /cards/from_node` — turn one Knowledge Store concept into a flashcard.
//!
//! This closes the loop that `/socratic/{id}/end` opens. That endpoint ships a
//! finished dialogue to the Knowledge Store, where extraction turns it into
//! concept nodes; this one takes a node the learner has actually studied and
//! makes it reviewable on the FSRS schedule. Source material therefore comes
//! from what the learner has covered, not from free text typed at generation
//! time — which is what separates this from
//! [`crate::handlers::generate`].
//!
//! ## One node, one card
//!
//! A call names a single node and produces a single card. `cards` carries a
//! `UNIQUE (set_id, source_node_id)` constraint (migration 0005), so calling
//! twice for the same concept in the same set does not quietly grow a pile of
//! near-duplicate cards — the second call reports the card that already exists.
//! Deduplication is on the node id rather than on the generated text because
//! the model phrases the same concept differently every time; comparing text
//! would never match.

use actix_web::{post, web, HttpResponse};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

use crate::ks_client::{KsClient, KsNode};
use crate::llm_provider::{LLMMessage, LLMProvider};
use super::{describe_llm_failure, error_response};

/// Value of `cards.source` written by this handler.
const SOURCE_KNOWLEDGE_STORE: &str = "knowledge_store";

#[derive(Debug, Deserialize)]
pub struct FromNodeRequest {
    pub study_set_id: Uuid,
    pub node_id: Uuid,
}

#[derive(Debug, Serialize, FromRow)]
pub struct CardOut {
    pub id: Uuid,
    pub set_id: Uuid,
    pub question: String,
    pub answer: String,
    pub source: String,
    pub source_node_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
pub struct FromNodeResponse {
    #[serde(flatten)]
    pub card: CardOut,
    pub tokens_used: u32,
}

/// 409 body. It carries the id of the card that already exists rather than a
/// bare error string: the caller's next move is almost always to look at that
/// card, and making them go search for it would be a pointless round trip.
#[derive(Debug, Serialize)]
struct AlreadyExistsResponse {
    error: String,
    existing_card_id: Uuid,
}

#[derive(Debug, FromRow)]
struct ExistingCardRow {
    id: Uuid,
}

/// The question/answer pair as the model is asked to emit it.
#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
struct GeneratedCard {
    question: String,
    answer: String,
}

/// Defensive parse: try the raw text, then strip a ```json fence. Same
/// leniency as `generate.rs`, `socratic.rs` and `quiz.rs`, for the same
/// reason — the model intermittently fences its JSON despite instructions.
fn parse_generated_card(raw: &str) -> Result<GeneratedCard, String> {
    if let Ok(v) = serde_json::from_str::<GeneratedCard>(raw) {
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
    serde_json::from_str::<GeneratedCard>(stripped)
        .map_err(|e| format!("{e} (after stripping fences)"))
}

/// Reject a card the learner could not use. A blank side is worse than a
/// missing card: it enters the review queue and wastes the reviewer's turn
/// before anyone notices it is empty.
fn validate_generated(card: &GeneratedCard) -> Result<(), String> {
    if card.question.trim().is_empty() {
        return Err("model returned an empty question".to_string());
    }
    if card.answer.trim().is_empty() {
        return Err("model returned an empty answer".to_string());
    }
    Ok(())
}

/// Build the prompt that turns a node into a flashcard.
///
/// Deliberately elaboration-shaped rather than the short-recall style
/// `generate.rs` uses by default: a node exists because the learner worked
/// through the idea in dialogue, so the useful question is one that asks them
/// to reconstruct the reasoning, not to name a term. The summary is given as
/// material to build on — the model is told not to quote it back, since a card
/// whose answer is the summary verbatim tests recognition of a sentence rather
/// than understanding of the concept.
fn build_prompt(node: &KsNode) -> (String, String) {
    let system = format!(
        "You are a flashcard author. Write exactly ONE question/answer pair \
         that tests real understanding of the concept the user provides — the \
         kind of question that asks WHY or HOW, or asks the learner to apply \
         the idea, not one that asks them to name a term. The answer should be \
         1-3 sentences, in the learner's own explanatory register.\n\
         \n\
         Write in the same language as the concept you are given.\n\
         \n\
         Do NOT quote the provided summary back as the answer: the learner has \
         already read it, and recognising a sentence is not the same as \
         understanding an idea.\n\
         \n\
         Respond with ONLY valid JSON, shaped \
         {{\"question\": string, \"answer\": string}}. No prose, no markdown \
         fences, no commentary outside the JSON.",
    );
    let user = format!(
        "Concept: {}\nSubject: {}\nWhat the learner worked out about it: {}\n\n\
         Write one question/answer pair.",
        node.title, node.subject, node.summary
    );
    (system, user)
}

#[post("/cards/from_node")]
pub async fn from_node(
    pool: web::Data<PgPool>,
    llm: web::Data<Box<dyn LLMProvider>>,
    ks: web::Data<Option<KsClient>>,
    body: web::Json<FromNodeRequest>,
) -> HttpResponse {
    // 1. The study set must exist. Its owner is also the user_id the
    //    ai_interactions row needs (NOT NULL, FK to users).
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

    // 2. A missing KS client is a configuration state, not a client error —
    //    the same distinction quiz.rs draws.
    let Some(ks) = ks.get_ref().as_ref() else {
        return error_response(
            actix_web::http::StatusCode::SERVICE_UNAVAILABLE,
            "Knowledge Store is not configured (KS_HTTP_TOKEN unset); \
             cannot build a card from a concept node",
        );
    };

    // 3. Check for an existing card BEFORE calling the model. The UNIQUE
    //    constraint in step 7 is what actually guarantees uniqueness, but
    //    catching the common case here means a repeat call costs no tokens.
    match existing_card(pool.get_ref(), body.study_set_id, body.node_id).await {
        Ok(Some(existing)) => return already_exists(body.node_id, existing.id),
        Ok(None) => {}
        Err(e) => {
            return error_response(
                actix_web::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("database error checking for an existing card: {e}"),
            );
        }
    }

    // 4. Fetch the node itself.
    let node = match ks.get_node(body.node_id).await {
        Ok(Some(n)) => n,
        Ok(None) => {
            return error_response(
                actix_web::http::StatusCode::NOT_FOUND,
                format!(
                    "the Knowledge Store has no concept node {} — only accepted \
                     concepts are available; one still pending review is not",
                    body.node_id
                ),
            );
        }
        Err(e) => {
            eprintln!("[ks] node lookup failed for {}: {e}", body.node_id);
            return error_response(
                actix_web::http::StatusCode::BAD_GATEWAY,
                format!("could not read the concept from the Knowledge Store: {e}"),
            );
        }
    };

    // 5. Generate.
    let (system_prompt, user_prompt) = build_prompt(&node);
    let messages = vec![
        LLMMessage::system(system_prompt.clone()),
        LLMMessage::user(user_prompt.clone()),
    ];
    let prompt_log = format!(
        "[card_from_node: {}]\n[system] {system_prompt}\n[user] {user_prompt}",
        node.id
    );

    let resp = match llm.chat_completion(&messages, None).await {
        Ok(r) => r,
        Err(api_err) => {
            // Covers truncation too: the provider rejects finish_reason=length
            // rather than handing back a half-written answer.
            let (placeholder, message) = describe_llm_failure(&api_err);
            let _ =
                log_ai_interaction(pool.get_ref(), owner.user_id, &prompt_log, &placeholder, 0)
                    .await;
            return error_response(actix_web::http::StatusCode::BAD_GATEWAY, message);
        }
    };
    let raw = resp.content;

    let generated = match parse_generated_card(&raw) {
        Ok(c) => c,
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

    if let Err(validation_err) = validate_generated(&generated) {
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
            format!("DeepSeek returned an unusable card; nothing inserted: {validation_err}"),
        );
    }

    // 6. Insert. ON CONFLICT DO NOTHING rather than a plain insert: two
    //    requests for the same node can race past the step-3 check, and the
    //    constraint is the only thing that actually settles it.
    let inserted: Option<CardOut> = match sqlx::query_as::<_, CardOut>(
        r#"INSERT INTO cards (set_id, question, answer, source, source_node_id)
           VALUES ($1, $2, $3, $4, $5)
           ON CONFLICT (set_id, source_node_id) DO NOTHING
           RETURNING id, set_id, question, answer, source, source_node_id, created_at"#,
    )
    .bind(body.study_set_id)
    .bind(&generated.question)
    .bind(&generated.answer)
    .bind(SOURCE_KNOWLEDGE_STORE)
    .bind(body.node_id)
    .fetch_optional(pool.get_ref())
    .await
    {
        Ok(row) => row,
        Err(e) => {
            // The call cost real tokens whether or not the write landed.
            let _ = log_ai_interaction(
                pool.get_ref(),
                owner.user_id,
                &prompt_log,
                &raw,
                resp.total_tokens,
            )
            .await;
            let (status, message) = super::classify_db_error(&e);
            return error_response(status, message);
        }
    };

    // 7. Always log the interaction: the tokens were spent either way, and a
    //    generation thrown away by a race is exactly the kind of cost that
    //    should stay visible.
    let _ = log_ai_interaction(
        pool.get_ref(),
        owner.user_id,
        &prompt_log,
        &raw,
        resp.total_tokens,
    )
    .await;

    let Some(card) = inserted else {
        // Lost the race. Report the winner rather than a bare conflict.
        return match existing_card(pool.get_ref(), body.study_set_id, body.node_id).await {
            Ok(Some(existing)) => already_exists(body.node_id, existing.id),
            Ok(None) => error_response(
                actix_web::http::StatusCode::INTERNAL_SERVER_ERROR,
                "insert reported a conflict but no existing card could be found",
            ),
            Err(e) => error_response(
                actix_web::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("database error resolving the conflicting card: {e}"),
            ),
        };
    };

    HttpResponse::Created().json(FromNodeResponse {
        card,
        tokens_used: resp.total_tokens,
    })
}

#[derive(Debug, FromRow)]
struct StudySetOwnerRow {
    user_id: Uuid,
}

async fn existing_card(
    pool: &PgPool,
    set_id: Uuid,
    node_id: Uuid,
) -> Result<Option<ExistingCardRow>, sqlx::Error> {
    sqlx::query_as::<_, ExistingCardRow>(
        "SELECT id FROM cards WHERE set_id = $1 AND source_node_id = $2",
    )
    .bind(set_id)
    .bind(node_id)
    .fetch_optional(pool)
    .await
}

fn already_exists(node_id: Uuid, existing_card_id: Uuid) -> HttpResponse {
    HttpResponse::Conflict().json(AlreadyExistsResponse {
        error: format!("this study set already has a card for concept node {node_id}"),
        existing_card_id,
    })
}

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
           VALUES ($1, 'card_from_node_generation', $2, $3, $4)"#,
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

    fn node() -> KsNode {
        KsNode {
            id: "5248a55b-ca35-4a33-8479-09d0ec0a6784".to_string(),
            title: "Định luật II Newton".to_string(),
            subject: "Vật lý".to_string(),
            summary: "Học sinh đã sửa công thức từ F = m*v thành F = m*a.".to_string(),
        }
    }

    // -- parsing model output ------------------------------------------------

    #[test]
    fn parses_plain_json_object() {
        let parsed =
            parse_generated_card(r#"{"question":"Tại sao?","answer":"Vì gia tốc."}"#).unwrap();
        assert_eq!(parsed.question, "Tại sao?");
        assert_eq!(parsed.answer, "Vì gia tốc.");
    }

    #[test]
    fn parses_json_wrapped_in_a_markdown_fence() {
        let raw = "```json\n{\"question\":\"Tại sao?\",\"answer\":\"Vì gia tốc.\"}\n```";
        assert_eq!(parse_generated_card(raw).unwrap().question, "Tại sao?");
    }

    #[test]
    fn prose_reply_is_rejected_as_unparseable() {
        assert!(parse_generated_card("Here is your flashcard!").is_err());
    }

    #[test]
    fn an_array_of_cards_is_rejected() {
        // This endpoint asks for one card; a JSON array is the model ignoring
        // that, and silently taking the first element would hide the drift.
        assert!(parse_generated_card(r#"[{"question":"q","answer":"a"}]"#).is_err());
    }

    // -- validation ----------------------------------------------------------

    #[test]
    fn accepts_a_well_formed_card() {
        assert!(validate_generated(&GeneratedCard {
            question: "Tại sao gia tốc chứ không phải vận tốc?".into(),
            answer: "Vì lực gây ra thay đổi vận tốc.".into(),
        })
        .is_ok());
    }

    #[test]
    fn rejects_blank_question() {
        assert!(validate_generated(&GeneratedCard {
            question: "   ".into(),
            answer: "Vì lực gây ra thay đổi vận tốc.".into(),
        })
        .is_err());
    }

    #[test]
    fn rejects_blank_answer() {
        // A card with an empty side reaches the review queue and wastes the
        // learner's turn before anyone notices.
        assert!(validate_generated(&GeneratedCard {
            question: "Tại sao?".into(),
            answer: "".into(),
        })
        .is_err());
    }

    // -- prompt --------------------------------------------------------------

    #[test]
    fn prompt_carries_the_node_and_states_the_json_contract() {
        let (system, user) = build_prompt(&node());
        assert!(system.contains("\"question\": string"));
        assert!(system.contains("ONE question/answer pair"));
        assert!(user.contains("Định luật II Newton"));
        assert!(user.contains("Vật lý"));
        assert!(user.contains("F = m*a"));
    }

    #[test]
    fn prompt_forbids_parroting_the_summary_back() {
        // Otherwise the answer is the summary verbatim and the card tests
        // recognition of a sentence, not understanding of the concept.
        let (system, _) = build_prompt(&node());
        assert!(system.contains("Do NOT quote the provided summary"));
    }

    // -- response shape ------------------------------------------------------

    #[test]
    fn response_reports_provenance_and_flattens_the_card() {
        let body = serde_json::to_value(FromNodeResponse {
            card: CardOut {
                id: Uuid::nil(),
                set_id: Uuid::nil(),
                question: "q".into(),
                answer: "a".into(),
                source: SOURCE_KNOWLEDGE_STORE.into(),
                source_node_id: Some(Uuid::nil()),
                created_at: Utc::now(),
            },
            tokens_used: 123,
        })
        .unwrap();

        // Flattened: card fields sit at the top level beside tokens_used,
        // matching the documented response shape.
        assert_eq!(body["question"], "q");
        assert_eq!(body["source"], "knowledge_store");
        assert_eq!(body["tokens_used"], 123);
    }

    #[test]
    fn conflict_body_names_the_card_that_already_exists() {
        // A bare "already exists" would leave the caller hunting for it.
        let existing = Uuid::parse_str("090aaecf-d8a8-4ca9-9191-19075245ab84").unwrap();
        let body = serde_json::to_value(AlreadyExistsResponse {
            error: "…".into(),
            existing_card_id: existing,
        })
        .unwrap();
        assert_eq!(body["existing_card_id"], existing.to_string());
    }
}
