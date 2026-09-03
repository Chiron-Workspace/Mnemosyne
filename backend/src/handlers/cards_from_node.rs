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
//!
//! ## Telling the 502s apart
//!
//! Every `502` body carries a machine-readable `reason` beside the usual
//! `error` message, so a caller can branch on it instead of pattern-matching
//! prose. The three values differ in what a retry is worth:
//!
//! - `"truncated"` — the model ran out of token budget mid-answer
//!   ([`LLMError::Truncated`]). **Do not retry the same input**: the same
//!   prompt against the same budget runs out at the same place. Asking for less
//!   is the move that helps.
//! - `"provider_error"` — any other LLM-side failure: network, an upstream
//!   status, or output that arrived whole and could not be parsed or used.
//!   **Retry with a bound.** It may be transient, but it may equally be a
//!   lasting misconfiguration — an invalid API key answers 401 every time —
//!   so an unbounded retry loop would hammer a wall.
//! - `"knowledge_store_error"` — the concept could not be read from the
//!   Knowledge Store. **Safe to retry**, and usually worth it: this is a blip
//!   on the second network hop (caller → Mnemosyne → Knowledge Store) and
//!   clears on its own far more often than the other two.
//!
//! `reason` is local to this endpoint. The other AI handlers still return a
//! bare `{"error": …}` — the split exists because the Knowledge Store's
//! card-sync job needs to branch on it, and so far nothing else does.

use actix_web::{post, web, HttpResponse};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

use crate::ks_client::{KsClient, KsError, KsNode};
use crate::llm_provider::{LLMError, LLMMessage, LLMProvider};
use super::{describe_llm_failure, error_response};

/// Value of `cards.source` written by this handler.
const SOURCE_KNOWLEDGE_STORE: &str = "knowledge_store";

/// `reason` on a 502: the model ran out of token budget mid-answer.
const REASON_TRUNCATED: &str = "truncated";

/// `reason` on a 502: any other LLM-side failure.
const REASON_PROVIDER_ERROR: &str = "provider_error";

/// `reason` on a 502: the concept could not be read from the Knowledge Store.
const REASON_KNOWLEDGE_STORE_ERROR: &str = "knowledge_store_error";

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

/// 502 body. `error` stays the same human-readable message every other
/// endpoint returns; `reason` is the machine-readable half. Without it the
/// only way to tell a truncated generation from a network blip is to read the
/// message text, which is not something a caller should have to depend on.
#[derive(Debug, Serialize)]
struct UpstreamErrorResponse {
    error: String,
    reason: &'static str,
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
                // Careful about the phrasing: `ks.nodes` has no status column,
                // so there is no such thing as an unapproved node. A concept
                // awaiting review lives in `ks.extracted_concepts` and becomes
                // a node only once `ks.cli accept` runs — which is why its id
                // is simply not a node id yet.
                format!(
                    "the Knowledge Store has no concept node {} — a concept \
                     becomes a node only after `ks.cli accept` runs, so one \
                     still awaiting review has no node id yet",
                    body.node_id
                ),
            );
        }
        Err(e) => {
            eprintln!("[ks] node lookup failed for {}: {e}", body.node_id);
            return knowledge_store_error(&e);
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
            let failure = describe_llm_failure(&api_err);
            let _ = log_ai_interaction(
                pool.get_ref(),
                owner.user_id,
                &prompt_log,
                &failure.placeholder,
                failure.tokens_used,
            )
            .await;
            return upstream_error(failure.message, llm_failure_reason(&api_err));
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
            // Not truncation: the answer arrived whole, it just was not JSON.
            return upstream_error(
                format!("DeepSeek returned non-JSON output, parse failed: {parse_err}"),
                REASON_PROVIDER_ERROR,
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
        return upstream_error(
            format!("DeepSeek returned an unusable card; nothing inserted: {validation_err}"),
            REASON_PROVIDER_ERROR,
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

/// Which `reason` an [`LLMError`] maps to. Truncation is the only variant
/// singled out, because it is the only one where retrying the same input
/// unchanged is predictably pointless: the same prompt against the same budget
/// runs out at the same place. Everything else — a timeout, a 429, output that
/// would not parse — can plausibly go differently next time.
fn llm_failure_reason(err: &LLMError) -> &'static str {
    match err {
        LLMError::Truncated { .. } => REASON_TRUNCATED,
        _ => REASON_PROVIDER_ERROR,
    }
}

/// 502 for a Knowledge Store lookup that failed.
///
/// It gets its own `reason` rather than sharing `provider_error`, because the
/// two carry different odds and so deserve different retry policies. This one
/// is nearly always a blip on the second network hop — the caller reaches
/// Mnemosyne, Mnemosyne reaches the Knowledge Store — and clears by itself,
/// whereas a provider failure can just as easily be a lasting
/// misconfiguration. Folded together, a caller would have to pick one policy
/// and be wrong for the other half of the cases.
fn knowledge_store_error(err: &KsError) -> HttpResponse {
    upstream_error(
        format!("could not read the concept from the Knowledge Store: {err}"),
        REASON_KNOWLEDGE_STORE_ERROR,
    )
}

fn upstream_error(message: impl Into<String>, reason: &'static str) -> HttpResponse {
    HttpResponse::BadGateway().json(UpstreamErrorResponse {
        error: message.into(),
        reason,
    })
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
    use crate::llm_provider::LLMResponse;

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

    // -- why a 502 happened --------------------------------------------------

    /// A stand-in provider, so these tests go through the real
    /// [`LLMProvider`] boundary the handler calls rather than hand-building an
    /// [`LLMError`] that the trait might never actually produce.
    struct FakeProvider {
        outcome: fn() -> Result<LLMResponse, LLMError>,
    }

    #[async_trait::async_trait]
    impl LLMProvider for FakeProvider {
        async fn chat_completion(
            &self,
            _messages: &[LLMMessage],
            _model: Option<&str>,
        ) -> Result<LLMResponse, LLMError> {
            (self.outcome)()
        }
    }

    async fn parts_of(resp: HttpResponse) -> (u16, serde_json::Value) {
        let status = resp.status().as_u16();
        let bytes = actix_web::body::to_bytes(resp.into_body())
            .await
            .expect("the body should be readable");
        (
            status,
            serde_json::from_slice(&bytes).expect("an error body should be JSON"),
        )
    }

    #[tokio::test]
    async fn a_truncated_generation_is_reported_as_reason_truncated() {
        // finish_reason == "length" never reaches a handler as a success — the
        // provider rejects it — so what arrives here is LLMError::Truncated.
        let provider = FakeProvider {
            outcome: || {
                Err(LLMError::Truncated {
                    finish_reason: "length".into(),
                    total_tokens: 4096,
                })
            },
        };
        let err = provider
            .chat_completion(&[], None)
            .await
            .expect_err("this fake provider only fails");

        let failure = describe_llm_failure(&err);

        // What gets written to ai_interactions for this failure. The tokens
        // were spent — mostly on reasoning that never reached the output — so
        // the row must say so; logging 0 would make the most expensive kind of
        // failure the one that looks free.
        assert_eq!(failure.tokens_used, 4096);

        let (status, body) =
            parts_of(upstream_error(failure.message, llm_failure_reason(&err))).await;

        assert_eq!(status, 502);
        assert_eq!(body["reason"], "truncated");
        // The prose message is unchanged; `reason` is additive.
        assert!(
            body["error"].as_str().unwrap_or_default().contains("token limit"),
            "error: {}",
            body["error"]
        );
    }

    #[tokio::test]
    async fn output_that_will_not_parse_is_reported_as_reason_provider_error() {
        // The opposite case: the call succeeded, stopped normally and cost
        // tokens — the model simply answered in prose. Nothing about it is
        // worth telling apart from a network failure, since the advice to the
        // caller ("try again") is the same.
        let provider = FakeProvider {
            outcome: || {
                Ok(LLMResponse {
                    content: "Here is your flashcard!".to_string(),
                    total_tokens: 42,
                    finish_reason: "stop".to_string(),
                })
            },
        };
        let resp = provider
            .chat_completion(&[], None)
            .await
            .expect("a non-JSON answer is still a successful call");
        let parse_err =
            parse_generated_card(&resp.content).expect_err("prose is not a card");

        let (status, body) = parts_of(upstream_error(
            format!("DeepSeek returned non-JSON output, parse failed: {parse_err}"),
            REASON_PROVIDER_ERROR,
        ))
        .await;

        assert_eq!(status, 502);
        assert_eq!(body["reason"], "provider_error");
    }

    #[tokio::test]
    async fn a_failed_knowledge_store_lookup_gets_its_own_reason() {
        // Not an LLM failure at all, and the most retryable of the three: KS
        // was simply not answering on the second hop. Sharing provider_error
        // would tell the caller to give up after a couple of tries on the one
        // failure most likely to clear by itself.
        //
        // Driven through the same function the handler calls, with a real
        // KsError — KsClient is a concrete struct with a localhost-only base
        // URL, so faking get_node itself would mean standing up an HTTP
        // server for no added coverage of this mapping.
        let (status, body) = parts_of(knowledge_store_error(&KsError::Unreachable(
            "error sending request: connection refused".into(),
        )))
        .await;

        assert_eq!(status, 502);
        assert_eq!(body["reason"], "knowledge_store_error");
        assert!(
            body["error"]
                .as_str()
                .unwrap_or_default()
                .contains("connection refused"),
            "the cause should survive into the message: {}",
            body["error"]
        );
    }

    #[test]
    fn the_three_reasons_are_distinct() {
        // They only earn their keep if a caller can tell them apart.
        let all = [
            REASON_TRUNCATED,
            REASON_PROVIDER_ERROR,
            REASON_KNOWLEDGE_STORE_ERROR,
        ];
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }

    #[test]
    fn every_non_truncation_failure_maps_to_provider_error() {
        // A new LLMError variant defaults to "provider_error" (retryable),
        // which is the safe side to fail on: at worst the caller wastes a
        // retry, whereas a wrong "truncated" would suppress one that would
        // have worked.
        for err in [
            LLMError::Network("timeout".into()),
            LLMError::Http {
                status: 429,
                body: "rate limited".into(),
            },
            LLMError::Parse("bad json".into()),
        ] {
            assert_eq!(
                llm_failure_reason(&err),
                REASON_PROVIDER_ERROR,
                "unexpected reason for {err:?}"
            );
        }
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
