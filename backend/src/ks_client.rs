//! HTTP client for the Chiron Knowledge Store (KS).
//!
//! KS is a **separate Python service** (Postgres-backed) reachable over
//! localhost HTTP. It is not callable in-process from this Rust backend — the
//! two live in different runtimes — so every interaction goes through this
//! client.
//!
//! Scope is deliberately narrow: `GET /health`, `POST /transcripts`,
//! `GET /nodes` and `GET /nodes/{id}`. `POST /ingest` is **not**
//! implemented here. Per the agreed
//! design, Mnemosyne only ever ships raw transcripts; concept extraction is
//! KS's own job, run in-process on the Python side by a systemd timer.
//! `/ingest` exists to serve LexiFlash later and has no consumer in Mnemosyne.
//!
//! Error taxonomy mirrors the spirit of [`crate::llm_provider::LLMError`]
//! (network / http / parse) for consistency, but is a distinct type: KS is a
//! different domain than the LLM provider and the two should not be conflated.
//!
//! ## The three outcomes of `POST /transcripts`
//!
//! This is the subtle part. KS swallows its own DB errors and reports them as
//! `HTTP 200` with `ok: false`, so "the request failed" and "the write failed"
//! are *different* conditions that must not be collapsed:
//!
//! 1. [`KsError::Unreachable`] — timeout / connection refused. The KS process
//!    is not answering. Retrying after a backoff is reasonable.
//! 2. [`SaveTranscriptOutcome::KsDbUnavailable`] — KS answered, but its
//!    database is down. An immediate retry accomplishes nothing; log it and
//!    move on.
//! 3. [`SaveTranscriptOutcome::Saved`] — success.
//!
//! ## Idempotency
//!
//! KS writes with `INSERT ... ON CONFLICT (session_ref) DO NOTHING`. Re-sending
//! the **same** `session_ref` returns the original `transcript_id` and creates
//! no second row, which makes retry-after-timeout safe — as long as the caller
//! keeps the same `session_ref`. Never mint a fresh `session_ref` on retry.
//! Note that a repeat send with *different* `content` does not update the
//! stored record; the first write wins.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Default KS port. Overridable via `KS_HTTP_PORT`.
const DEFAULT_PORT: &str = "8080";

/// Page size requested when looking up a single node by id **on the fallback
/// path only** — see [`KsClient::get_node`].
///
/// Matches KS's `MAX_NODE_LIMIT`, the largest page it will serve — asking for
/// more is a 400, not a bigger page. The limit is stated rather than left at
/// KS's default of 50 because silently searching only the first 50 would
/// report a perfectly real node as missing.
const NODE_LOOKUP_LIMIT: u32 = 500;

/// The `error` code KS returns in the body of its own "no such node" 404.
const ERROR_NODE_NOT_FOUND: &str = "node_not_found";

/// `/transcripts` writes straight to Postgres with no LLM in the path, so a
/// short timeout is correct — there is nothing slow to wait for.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

/// One conversational turn as KS expects it.
///
/// KS accepts any valid JSON in the `content` field at the storage layer, but
/// its downstream extraction step only understands an array of
/// `{role, content}` objects using the `coach` / `learner` label pair. Always
/// send that shape — use [`TranscriptTurn::coach`] / [`TranscriptTurn::learner`]
/// rather than passing raw role strings through.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TranscriptTurn {
    pub role: String,
    pub content: String,
}

/// Role label for the AI tutor's turns, as KS's extraction step expects it.
pub const ROLE_COACH: &str = "coach";
/// Role label for the student's turns, as KS's extraction step expects it.
pub const ROLE_LEARNER: &str = "learner";

impl TranscriptTurn {
    pub fn coach(content: impl Into<String>) -> Self {
        Self { role: ROLE_COACH.to_string(), content: content.into() }
    }

    pub fn learner(content: impl Into<String>) -> Self {
        Self { role: ROLE_LEARNER.to_string(), content: content.into() }
    }

    /// Translate a `socratic_messages.role` value into the KS label pair.
    /// Mnemosyne stores `assistant` / `user`; KS extraction wants
    /// `coach` / `learner`. Unknown roles are dropped by the caller.
    pub fn from_socratic_role(role: &str, content: impl Into<String>) -> Option<Self> {
        match role {
            "assistant" => Some(Self::coach(content)),
            "user" => Some(Self::learner(content)),
            _ => None,
        }
    }
}

#[derive(Debug, Serialize)]
struct SaveTranscriptRequest<'a> {
    session_ref: &'a str,
    content: &'a [TranscriptTurn],
}

/// Raw `POST /transcripts` response body. `ok: false` carries `error` and a
/// null `transcript_id`; `ok: true` carries the id.
#[derive(Debug, Deserialize)]
struct SaveTranscriptBody {
    ok: bool,
    #[serde(default)]
    transcript_id: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct HealthBody {
    status: String,
}

// ---------------------------------------------------------------------------
// Outcome / error types
// ---------------------------------------------------------------------------

/// The two ways a *successfully delivered* `POST /transcripts` can turn out.
/// Transport-level failures are [`KsError`] instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SaveTranscriptOutcome {
    /// KS stored the transcript (or already had it under this `session_ref`).
    Saved { transcript_id: String },
    /// HTTP 200 with `ok: false` — KS is alive but its database is not.
    /// Retrying immediately will not help.
    KsDbUnavailable { error: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KsError {
    /// Timeout, connection refused, DNS/TLS — the KS process did not answer.
    Unreachable(String),
    /// A non-success status. For `/transcripts` this means 400 (bad JSON or a
    /// missing field), 401 (no `Authorization` header) or 403 (wrong token).
    Http { status: u16, body: String },
    /// The body did not match the documented schema.
    Parse(String),
}

impl std::fmt::Display for KsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KsError::Unreachable(m) => write!(f, "knowledge store unreachable: {m}"),
            KsError::Http { status, body } => {
                let snippet: String = body.chars().take(500).collect();
                write!(f, "knowledge store HTTP {status}: {snippet}")
            }
            KsError::Parse(m) => write!(f, "knowledge store parse error: {m}"),
        }
    }
}

impl std::error::Error for KsError {}

// ---------------------------------------------------------------------------
// Pure helpers (unit-testable without touching the network)
// ---------------------------------------------------------------------------

/// Percent-encode one query-string value.
///
/// Written out rather than pulled from a crate because reqwest's `query()`
/// helper is not available under this project's minimal feature set, and a
/// subject filter can legitimately contain spaces and non-ASCII text
/// ("Vật lý"), which must not be pasted into a URL raw. Only the unreserved
/// set from RFC 3986 survives untouched; everything else, including every
/// byte of a multi-byte UTF-8 character, is escaped.
fn percent_encode_query_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn base_url_for_port(port: &str) -> String {
    format!("http://127.0.0.1:{port}")
}

/// Build the query string for `GET /nodes`, including the leading `?` when
/// there is anything to send and nothing at all when there is not.
fn nodes_query(subject: Option<&str>, limit: Option<u32>) -> String {
    let mut params: Vec<String> = Vec::new();
    if let Some(subject) = subject {
        params.push(format!("subject={}", percent_encode_query_value(subject)));
    }
    if let Some(limit) = limit {
        params.push(format!("limit={limit}"));
    }
    if params.is_empty() {
        String::new()
    } else {
        format!("?{}", params.join("&"))
    }
}

/// Pick the node whose id equals `node_id`, comparing parsed UUIDs so that
/// textual differences (case, surrounding whitespace) cannot cause a false
/// miss. A node with an unparseable id is skipped, never loosely matched.
fn find_node_by_id(nodes: Vec<KsNode>, node_id: Uuid) -> Option<KsNode> {
    nodes
        .into_iter()
        .find(|n| Uuid::parse_str(n.id.trim()).is_ok_and(|id| id == node_id))
}

/// Map an HTTP status onto the error taxonomy. `Ok(())` means the body is
/// worth parsing.
///
/// `/transcripts` is documented never to return 5xx — KS catches its own DB
/// errors and reports them in-band as `ok: false`. A 5xx from that endpoint is
/// therefore a bug on the KS side; we surface it loudly rather than papering
/// over it.
fn check_status(status: u16, body: &str) -> Result<(), KsError> {
    if (200..300).contains(&status) {
        return Ok(());
    }
    if status >= 500 {
        eprintln!(
            "[ks] BUG on the KS side: HTTP {status} from an endpoint documented never to \
             return 5xx. Body: {}",
            body.chars().take(500).collect::<String>()
        );
    }
    Err(KsError::Http { status, body: body.to_string() })
}

/// Parse a 2xx `/transcripts` body into one of the two delivered outcomes.
fn parse_save_response(body: &str) -> Result<SaveTranscriptOutcome, KsError> {
    let parsed: SaveTranscriptBody = serde_json::from_str(body).map_err(|e| {
        KsError::Parse(format!(
            "{e}; body snippet: {}",
            body.chars().take(500).collect::<String>()
        ))
    })?;

    if parsed.ok {
        match parsed.transcript_id {
            Some(id) if !id.is_empty() => Ok(SaveTranscriptOutcome::Saved { transcript_id: id }),
            // Contract violation: ok:true must always carry an id.
            _ => Err(KsError::Parse(
                "response had ok:true but no transcript_id".to_string(),
            )),
        }
    } else {
        Ok(SaveTranscriptOutcome::KsDbUnavailable {
            error: parsed
                .error
                .unwrap_or_else(|| "ok:false with no error field".to_string()),
        })
    }
}

fn parse_health_response(body: &str) -> Result<(), KsError> {
    let parsed: HealthBody = serde_json::from_str(body).map_err(|e| {
        KsError::Parse(format!(
            "{e}; body snippet: {}",
            body.chars().take(500).collect::<String>()
        ))
    })?;
    if parsed.status == "ok" {
        Ok(())
    } else {
        Err(KsError::Parse(format!(
            "unexpected health status: {}",
            parsed.status
        )))
    }
}

/// One concept node from the Knowledge Store, as `GET /nodes` returns it.
///
/// Mirrors the columns of `ks.nodes` that Mnemosyne actually needs. Quiz
/// generation reads `title` and `summary` as the source material for a
/// question, and keeps `id` so the resulting question can be traced back to
/// the node it came from.
///
/// Unknown fields are ignored rather than rejected, so KS can add columns to
/// its own response without breaking this client.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct KsNode {
    pub id: String,
    pub title: String,
    pub subject: String,
    pub summary: String,
}

/// Envelope form of the `GET /nodes` response: `{"nodes": [...]}`.
#[derive(Debug, Deserialize)]
struct NodesEnvelope {
    nodes: Vec<KsNode>,
}

/// Parse a `GET /nodes` body.
///
/// The route is live and returns the envelope form, `{"nodes": [...]}` —
/// confirmed against a running KS. The bare-array branch is left in place from
/// when the contract was still unknown: it costs one fallback attempt on a
/// body that would otherwise be an error anyway, and removing it would buy
/// nothing but a narrower client.
fn parse_nodes_response(body: &str) -> Result<Vec<KsNode>, KsError> {
    if let Ok(envelope) = serde_json::from_str::<NodesEnvelope>(body) {
        return Ok(envelope.nodes);
    }
    serde_json::from_str::<Vec<KsNode>>(body).map_err(|e| {
        KsError::Parse(format!(
            "{e}; expected either {{\"nodes\": [...]}} or a bare array; body snippet: {}",
            body.chars().take(500).collect::<String>()
        ))
    })
}

/// Parse a `GET /nodes/{id}` body: a bare node object.
///
/// Strict where [`parse_nodes_response`] is lenient. That route's shape was
/// unknown when it was written; this one arrives with a contract that states
/// the object is *not* wrapped in `{"node": ...}`, so a wrapped body would be
/// KS breaking its own contract — worth surfacing as a parse error rather than
/// quietly accommodating.
fn parse_node_response(body: &str) -> Result<KsNode, KsError> {
    serde_json::from_str::<KsNode>(body).map_err(|e| {
        KsError::Parse(format!(
            "{e}; expected a bare node object from GET /nodes/{{id}}; body snippet: {}",
            body.chars().take(500).collect::<String>()
        ))
    })
}

/// The `{"error": "...", "detail": "..."}` shape KS uses for its own errors.
#[derive(Debug, Deserialize)]
struct KsErrorBody {
    error: String,
}

/// What a `GET /nodes/{id}` call established.
#[derive(Debug)]
enum ByIdOutcome {
    Found(KsNode),
    /// KS answered its documented 404: it has no such node.
    NotFound,
    /// The 404 came from the web framework, not from KS — the route is not
    /// deployed on the KS this client is talking to.
    RouteAbsent,
}

/// Tell KS's "no such node" apart from Flask's "no such route". Both are 404s
/// and they mean opposite things: the first is a final answer, the second says
/// to ask a different way.
///
/// The tell is the body. KS answers with JSON naming the error; Werkzeug
/// answers with an HTML page. Either misreading is harmless — a JSON 404 we
/// failed to recognise merely costs one extra list call that reaches the same
/// `None` — so the heuristic cannot produce a wrong answer, only a slower one.
fn classify_not_found(body: &str) -> ByIdOutcome {
    match serde_json::from_str::<KsErrorBody>(body) {
        Ok(parsed) if parsed.error == ERROR_NODE_NOT_FOUND => ByIdOutcome::NotFound,
        _ => ByIdOutcome::RouteAbsent,
    }
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

pub struct KsClient {
    http: reqwest::Client,
    base_url: String,
    token: String,
}

impl KsClient {
    /// Build a client from the environment: `KS_HTTP_TOKEN` (required) and
    /// `KS_HTTP_PORT` (optional, defaults to 8080).
    ///
    /// Returns `None` when the token is absent or empty. **Callers must not
    /// panic on `None`.** Unlike `DEEPSEEK_API_KEY` — where the AI tutor is the
    /// core feature and a fast, loud failure is right — KS is auxiliary
    /// bookkeeping. A study session must run perfectly well without it.
    pub fn from_env() -> Option<Self> {
        let token = std::env::var("KS_HTTP_TOKEN").ok()?;
        if token.is_empty() {
            return None;
        }
        let port = std::env::var("KS_HTTP_PORT").unwrap_or_else(|_| DEFAULT_PORT.to_string());
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .expect("reqwest client construction should not fail with sane defaults");
        Some(Self { http, base_url: base_url_for_port(&port), token })
    }

    /// `GET /health` — no auth required.
    ///
    /// Worth calling before blaming the token: a failure here says the KS
    /// process is down, whereas a 403 on `/transcripts` with a healthy
    /// `/health` says the token is wrong.
    pub async fn health(&self) -> Result<(), KsError> {
        let resp = self
            .http
            .get(format!("{}/health", self.base_url))
            .send()
            .await
            .map_err(|e| KsError::Unreachable(e.to_string()))?;

        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        check_status(status, &body)?;
        parse_health_response(&body)
    }

    /// `POST /transcripts` — ship one finished session's raw transcript.
    ///
    /// `session_ref` is the idempotency key and is UNIQUE in KS's schema. Derive
    /// it from a stable Mnemosyne identifier (the Socratic session UUID) so a
    /// retry re-sends the *same* value and KS deduplicates instead of storing a
    /// duplicate.
    pub async fn save_transcript(
        &self,
        session_ref: &str,
        content: &[TranscriptTurn],
    ) -> Result<SaveTranscriptOutcome, KsError> {
        let req = SaveTranscriptRequest { session_ref, content };

        let resp = self
            .http
            .post(format!("{}/transcripts", self.base_url))
            .bearer_auth(&self.token)
            .json(&req)
            .send()
            .await
            .map_err(|e| KsError::Unreachable(e.to_string()))?;

        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        check_status(status, &body)?;
        parse_save_response(&body)
    }

    /// `GET /nodes` — list concept nodes the learner has already studied,
    /// optionally narrowed to one subject.
    ///
    /// Used as source material for quiz generation: a question is built from a
    /// node's `title` and `summary` so the learner is tested on what they have
    /// actually covered, rather than on free text they typed just now.
    ///
    /// Returns however many nodes KS's own default limit allows. Callers that
    /// need a specific node should use [`KsClient::get_node`] instead of
    /// scanning this themselves.
    pub async fn get_nodes(&self, subject: Option<&str>) -> Result<Vec<KsNode>, KsError> {
        self.fetch_nodes(subject, None).await
    }

    /// Fetch one node by id.
    ///
    /// Prefers `GET /nodes/{id}`, and falls back to listing nodes and picking
    /// the match here when that route is not deployed on the KS being talked
    /// to. The fallback is what this method used to do outright; it stays only
    /// until the by-id route is confirmed live, because the two paths are not
    /// equivalent:
    ///
    /// - The by-id route **resolves merges**: ask for a node that was merged
    ///   into another and KS answers 200 with the surviving node, whose `id`
    ///   is therefore *not* the id that was asked for. This method returns
    ///   that node unchanged — callers must not assume `node.id` echoes their
    ///   argument.
    /// - The fallback cannot do that. It matches an id exactly, so a merged-away
    ///   id comes back as `None` there and as the surviving node here. That
    ///   divergence is the reason not to leave the fallback in place any longer
    ///   than it takes KS to ship the route.
    /// - The fallback is also bounded by [`NODE_LOOKUP_LIMIT`]: past that many
    ///   nodes it would report a real node as missing.
    ///
    /// On the fallback path ids are compared as parsed UUIDs, not as strings,
    /// so formatting differences on either side cannot cause a false miss. A
    /// node whose id does not parse is skipped rather than matched loosely.
    pub async fn get_node(&self, node_id: Uuid) -> Result<Option<KsNode>, KsError> {
        match self.fetch_node_by_id(node_id).await? {
            ByIdOutcome::Found(node) => Ok(Some(node)),
            ByIdOutcome::NotFound => Ok(None),
            ByIdOutcome::RouteAbsent => {
                let nodes = self.fetch_nodes(None, Some(NODE_LOOKUP_LIMIT)).await?;
                Ok(find_node_by_id(nodes, node_id))
            }
        }
    }

    /// One `GET /nodes/{id}` call. A 404 is not an error here — it is an
    /// answer, and [`classify_not_found`] decides which of the two answers it
    /// is. Every other non-2xx status goes through [`check_status`] like the
    /// rest of the client: 403 for a bad token, 503 when KS's database is
    /// down, 400 for an id KS rejects (which this client should never send,
    /// since it formats a parsed [`Uuid`]).
    async fn fetch_node_by_id(&self, node_id: Uuid) -> Result<ByIdOutcome, KsError> {
        let resp = self
            .http
            .get(format!("{}/nodes/{node_id}", self.base_url))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| KsError::Unreachable(e.to_string()))?;

        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();

        if status == 404 {
            return Ok(classify_not_found(&body));
        }
        if status == 400 {
            // `invalid_node_id` — new on this route compared to `GET /nodes`,
            // and unreachable from here: the id is formatted from a parsed
            // `Uuid`. If it ever fires, the bug is on this side, so say so
            // rather than leaving it to look like a KS fault. The body (which
            // names the code) still reaches the caller through `check_status`.
            eprintln!(
                "[ks] BUG on the Mnemosyne side: KS rejected {node_id} as not a UUID. Body: {}",
                body.chars().take(200).collect::<String>()
            );
        }
        check_status(status, &body)?;
        parse_node_response(&body).map(ByIdOutcome::Found)
    }

    async fn fetch_nodes(
        &self,
        subject: Option<&str>,
        limit: Option<u32>,
    ) -> Result<Vec<KsNode>, KsError> {
        let url = format!("{}/nodes{}", self.base_url, nodes_query(subject, limit));

        let resp = self
            .http
            .get(url)
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| KsError::Unreachable(e.to_string()))?;

        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        check_status(status, &body)?;
        parse_nodes_response(&body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- POST /transcripts response parsing ---------------------------------

    #[test]
    fn parses_ok_true_into_saved() {
        let body = r#"{"ok": true, "transcript_id": "298178c8-1a0c-4d2c-adce-b143f6247a30"}"#;
        assert_eq!(
            parse_save_response(body).unwrap(),
            SaveTranscriptOutcome::Saved {
                transcript_id: "298178c8-1a0c-4d2c-adce-b143f6247a30".to_string()
            }
        );
    }

    #[test]
    fn parses_ok_false_into_ks_db_unavailable_not_an_error() {
        // HTTP 200 + ok:false is KS telling us *its* DB is down. It must not
        // be collapsed into the transport-error path.
        let body = r#"{"ok": false, "transcript_id": null, "error": "connection refused"}"#;
        assert_eq!(
            parse_save_response(body).unwrap(),
            SaveTranscriptOutcome::KsDbUnavailable {
                error: "connection refused".to_string()
            }
        );
    }

    #[test]
    fn ok_false_without_error_field_still_reports_db_unavailable() {
        let body = r#"{"ok": false}"#;
        assert!(matches!(
            parse_save_response(body).unwrap(),
            SaveTranscriptOutcome::KsDbUnavailable { .. }
        ));
    }

    #[test]
    fn ok_true_without_transcript_id_is_a_parse_error() {
        let body = r#"{"ok": true, "transcript_id": null}"#;
        assert!(matches!(parse_save_response(body), Err(KsError::Parse(_))));
    }

    #[test]
    fn malformed_body_is_a_parse_error() {
        assert!(matches!(
            parse_save_response("<html>502 Bad Gateway</html>"),
            Err(KsError::Parse(_))
        ));
    }

    // -- status mapping ------------------------------------------------------

    #[test]
    fn success_status_passes_through() {
        assert!(check_status(200, r#"{"ok": true}"#).is_ok());
    }

    #[test]
    fn bad_request_maps_to_http_400() {
        let err = check_status(400, "missing session_ref").unwrap_err();
        assert_eq!(
            err,
            KsError::Http { status: 400, body: "missing session_ref".to_string() }
        );
    }

    #[test]
    fn missing_auth_header_maps_to_http_401() {
        let err = check_status(401, "unauthorized").unwrap_err();
        assert!(matches!(err, KsError::Http { status: 401, .. }));
    }

    #[test]
    fn wrong_token_maps_to_http_403() {
        let err = check_status(403, "forbidden").unwrap_err();
        assert!(matches!(err, KsError::Http { status: 403, .. }));
    }

    #[test]
    fn server_error_is_surfaced_not_swallowed() {
        // /transcripts is documented never to return 5xx; if it does, that is a
        // KS bug and must reach the caller rather than be silently handled.
        let err = check_status(500, "traceback...").unwrap_err();
        assert!(matches!(err, KsError::Http { status: 500, .. }));
    }

    // -- /health -------------------------------------------------------------

    #[test]
    fn health_accepts_documented_body() {
        assert!(parse_health_response(r#"{"status": "ok"}"#).is_ok());
    }

    #[test]
    fn health_rejects_unexpected_status_value() {
        assert!(matches!(
            parse_health_response(r#"{"status": "degraded"}"#),
            Err(KsError::Parse(_))
        ));
    }

    // -- request shape -------------------------------------------------------

    #[test]
    fn request_serializes_to_the_documented_shape() {
        let turns = vec![
            TranscriptTurn::coach("Định luật Newton 2 phát biểu thế nào?"),
            TranscriptTurn::learner("Gia tốc tỉ lệ thuận với lực, F = ma."),
        ];
        let req = SaveTranscriptRequest {
            session_ref: "mnemosyne-session-8f21ac",
            content: &turns,
        };
        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&req).unwrap()).unwrap();

        assert_eq!(json["session_ref"], "mnemosyne-session-8f21ac");
        assert_eq!(json["content"][0]["role"], "coach");
        assert_eq!(
            json["content"][0]["content"],
            "Định luật Newton 2 phát biểu thế nào?"
        );
        assert_eq!(json["content"][1]["role"], "learner");
    }

    #[test]
    fn socratic_roles_map_onto_the_coach_learner_pair() {
        assert_eq!(
            TranscriptTurn::from_socratic_role("assistant", "q?"),
            Some(TranscriptTurn::coach("q?"))
        );
        assert_eq!(
            TranscriptTurn::from_socratic_role("user", "a."),
            Some(TranscriptTurn::learner("a."))
        );
        assert_eq!(TranscriptTurn::from_socratic_role("system", "x"), None);
    }

    // -- config --------------------------------------------------------------

    #[test]
    fn query_values_are_percent_encoded() {
        assert_eq!(percent_encode_query_value("physics"), "physics");
        assert_eq!(percent_encode_query_value("earth science"), "earth%20science");
        // Non-ASCII subjects are the normal case in this project, not an edge.
        assert_eq!(percent_encode_query_value("Vật lý"), "V%E1%BA%ADt%20l%C3%BD");
        assert_eq!(percent_encode_query_value("a&b=c"), "a%26b%3Dc");
    }

    #[test]
    fn base_url_is_localhost_only() {
        assert_eq!(base_url_for_port("8080"), "http://127.0.0.1:8080");
        assert_eq!(base_url_for_port("9999"), "http://127.0.0.1:9999");
    }

    // -- GET /nodes ----------------------------------------------------------

    #[test]
    fn parses_nodes_envelope_shape() {
        let body = r#"{"nodes": [
            {"id": "a64b3349-e3b6-4a19-b1bd-f024c2adc31d",
             "title": "Định luật Newton 2",
             "subject": "Vật lý",
             "summary": "Gia tốc tỉ lệ thuận với lực và tỉ lệ nghịch với khối lượng."}
        ]}"#;
        let nodes = parse_nodes_response(body).unwrap();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].title, "Định luật Newton 2");
        assert_eq!(nodes[0].subject, "Vật lý");
    }

    #[test]
    fn parses_nodes_bare_array_shape() {
        // The route's contract is not fixed yet, so both shapes must work.
        let body = r#"[
            {"id": "1", "title": "T", "subject": "S", "summary": "Sum"}
        ]"#;
        let nodes = parse_nodes_response(body).unwrap();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].id, "1");
    }

    #[test]
    fn nodes_response_ignores_unknown_fields() {
        // KS must be free to add columns without breaking this client.
        let body = r#"{"nodes": [{"id": "1", "title": "T", "subject": "S",
            "summary": "Sum", "source_module": "mnemosyne",
            "merged_into_id": null, "created_at": "2026-08-27T00:00:00Z"}]}"#;
        assert_eq!(parse_nodes_response(body).unwrap().len(), 1);
    }

    #[test]
    fn empty_nodes_list_is_not_an_error() {
        // A learner with nothing studied yet is a normal state, not a failure.
        assert!(parse_nodes_response(r#"{"nodes": []}"#).unwrap().is_empty());
    }

    // -- single-node lookup --------------------------------------------------

    fn node(id: &str, title: &str) -> KsNode {
        KsNode {
            id: id.to_string(),
            title: title.to_string(),
            subject: "Vật lý".to_string(),
            summary: "…".to_string(),
        }
    }

    const NODE_A: &str = "5248a55b-ca35-4a33-8479-09d0ec0a6784";
    const NODE_B: &str = "8bea853e-1cea-4114-a3b5-e77b487b88b9";

    #[test]
    fn lookup_finds_the_requested_node() {
        let nodes = vec![node(NODE_A, "Định luật II Newton"), node(NODE_B, "Gia tốc")];
        let found = find_node_by_id(nodes, Uuid::parse_str(NODE_B).unwrap()).unwrap();
        assert_eq!(found.title, "Gia tốc");
    }

    #[test]
    fn lookup_of_an_absent_node_is_none_not_an_error() {
        // A caller asking for a node that was never studied is a 404, not a
        // failure of the client.
        let nodes = vec![node(NODE_A, "Định luật II Newton")];
        assert!(find_node_by_id(nodes, Uuid::parse_str(NODE_B).unwrap()).is_none());
    }

    #[test]
    fn lookup_compares_uuids_not_raw_strings() {
        // Same id, different text. A string comparison would miss this.
        let nodes = vec![node(&NODE_A.to_uppercase(), "Định luật II Newton")];
        assert!(find_node_by_id(nodes, Uuid::parse_str(NODE_A).unwrap()).is_some());
    }

    #[test]
    fn lookup_skips_a_node_whose_id_does_not_parse() {
        let nodes = vec![node("not-a-uuid", "rác"), node(NODE_A, "Định luật II Newton")];
        let found = find_node_by_id(nodes, Uuid::parse_str(NODE_A).unwrap()).unwrap();
        assert_eq!(found.title, "Định luật II Newton");
    }

    // -- query building ------------------------------------------------------

    #[test]
    fn nodes_query_is_empty_when_nothing_is_constrained() {
        assert_eq!(nodes_query(None, None), "");
    }

    #[test]
    fn nodes_query_states_the_limit_for_a_by_id_lookup() {
        // The limit must be explicit: KS defaults to 50, and searching only
        // the first 50 would report a real node as missing.
        assert_eq!(nodes_query(None, Some(NODE_LOOKUP_LIMIT)), "?limit=500");
    }

    #[test]
    fn nodes_query_encodes_subject_and_combines_params() {
        assert_eq!(
            nodes_query(Some("Vật lý"), Some(10)),
            "?subject=V%E1%BA%ADt%20l%C3%BD&limit=10"
        );
    }

    #[test]
    fn malformed_nodes_body_is_a_parse_error() {
        assert!(matches!(
            parse_nodes_response("<html>404</html>"),
            Err(KsError::Parse(_))
        ));
    }

    // -- live end-to-end check ----------------------------------------------
    //
    // #[ignore] by default so the normal `cargo test` run stays offline and
    // deterministic. Run it deliberately, once KS is up and KS_HTTP_TOKEN is
    // set in .env:
    //
    //     cargo test -p backend -- --ignored --nocapture ks_client::tests::live
    //
    // It exercises the same client code the server uses, so a pass here is a
    // genuine Mnemosyne -> Knowledge Store call and the printed transcript_id
    // can be correlated against `journalctl -u chiron-ks-http.service`.

    #[tokio::test]
    #[ignore = "requires a running Knowledge Store and a real KS_HTTP_TOKEN"]
    async fn live_health_and_save_transcript() {
        // cargo runs test binaries with the package directory as CWD, so look
        // for .env at the workspace root as well.
        dotenvy::dotenv().ok();
        dotenvy::from_filename("../.env").ok();

        let client = KsClient::from_env()
            .expect("KS_HTTP_TOKEN must be set in .env to run this test");

        client.health().await.expect("KS /health should succeed");
        eprintln!("[live] /health ok");

        // A fixed session_ref, so re-running this test exercises the
        // ON CONFLICT (session_ref) DO NOTHING path rather than accumulating
        // junk rows: the second run must return the same transcript_id.
        let session_ref = "mnemosyne-session-live-check";
        let turns = vec![
            TranscriptTurn::coach("Định luật Newton 2 phát biểu thế nào?"),
            TranscriptTurn::learner("Gia tốc tỉ lệ thuận với lực, F = ma."),
        ];

        match client.save_transcript(session_ref, &turns).await {
            Ok(SaveTranscriptOutcome::Saved { transcript_id }) => {
                eprintln!("[live] saved: session_ref={session_ref} transcript_id={transcript_id}");

                let again = client.save_transcript(session_ref, &turns).await.unwrap();
                assert_eq!(
                    again,
                    SaveTranscriptOutcome::Saved { transcript_id: transcript_id.clone() },
                    "re-sending the same session_ref must return the same transcript_id"
                );
                eprintln!("[live] idempotency confirmed: {transcript_id}");
            }
            Ok(SaveTranscriptOutcome::KsDbUnavailable { error }) => {
                panic!("KS is up but its database is unavailable: {error}");
            }
            Err(e) => panic!("KS call failed: {e}"),
        }
    }

    // -- GET /nodes/{id} -----------------------------------------------------

    #[test]
    fn by_id_response_is_a_bare_node_object() {
        let node = parse_node_response(
            r#"{"id":"5248a55b-ca35-4a33-8479-09d0ec0a6784","title":"Định luật II Newton",
                "subject":"Vật lý","summary":"F = m*a."}"#,
        )
        .expect("the documented 200 shape should parse");

        assert_eq!(node.title, "Định luật II Newton");
        assert_eq!(node.subject, "Vật lý");
    }

    #[test]
    fn by_id_response_wrapped_in_an_envelope_is_a_parse_error() {
        // The contract states the object is returned bare. A wrapped body
        // would be KS breaking it, and accommodating that quietly here would
        // hide the drift from both sides.
        assert!(parse_node_response(
            r#"{"node":{"id":"5248a55b-ca35-4a33-8479-09d0ec0a6784","title":"t",
                "subject":"s","summary":"x"}}"#
        )
        .is_err());
    }

    #[test]
    fn by_id_response_keeps_the_id_ks_returned() {
        // Merge resolution: asking for a node that was merged away answers 200
        // with the surviving node, so the id that comes back is deliberately
        // not the id that was asked for. Nothing here may "correct" it.
        let merged_into = "8bea853e-1cea-4114-a3b5-e77b487b88b9";
        let node = parse_node_response(&format!(
            r#"{{"id":"{merged_into}","title":"t","subject":"s","summary":"x"}}"#
        ))
        .unwrap();

        assert_eq!(node.id, merged_into);
    }

    #[test]
    fn a_json_404_from_ks_means_the_node_does_not_exist() {
        assert!(matches!(
            classify_not_found(r#"{"error":"node_not_found","detail":"no node with that id"}"#),
            ByIdOutcome::NotFound
        ));
    }

    #[test]
    fn an_html_404_means_the_route_is_not_deployed() {
        // Captured verbatim from the running KS before the route shipped:
        // Werkzeug's default page, served with Content-Type text/html. Read as
        // a real miss it would turn "this KS is older than the client" into
        // "your concept does not exist".
        let werkzeug = "<!doctype html>\n<html lang=en>\n<title>404 Not Found</title>\n                        <h1>Not Found</h1>\n<p>The requested URL was not found on the server.</p>";
        assert!(matches!(
            classify_not_found(werkzeug),
            ByIdOutcome::RouteAbsent
        ));
    }

    #[test]
    fn an_unrecognised_json_404_falls_back_rather_than_guessing() {
        // Misreading in this direction is the harmless one: the fallback list
        // scan reaches the same `None`, one wasted call later.
        assert!(matches!(
            classify_not_found(r#"{"error":"something_else"}"#),
            ByIdOutcome::RouteAbsent
        ));
    }

    /// Proves this client against the real `GET /nodes/{id}` route. A fixture
    /// can only show we handle a body we invented; this shows the route
    /// answers the way the contract says and that a lookup does not silently
    /// fall through to the list scan.
    ///
    ///     cargo test -p backend -- --ignored --nocapture live_get_node_by_id
    #[tokio::test]
    #[ignore = "requires a running Knowledge Store and a real KS_HTTP_TOKEN"]
    async fn live_get_node_by_id() {
        dotenvy::dotenv().ok();
        dotenvy::from_filename("../.env").ok();

        let client = KsClient::from_env().expect("KS_HTTP_TOKEN must be set in .env");

        // Every node KS holds must be reachable one at a time.
        let listed = client.get_nodes(None).await.expect("GET /nodes should work");
        assert!(!listed.is_empty(), "KS has no nodes to look up");
        for expected in &listed {
            let id = Uuid::parse_str(&expected.id).expect("KS ids should be UUIDs");
            let found = client
                .get_node(id)
                .await
                .expect("a listed node must be fetchable by id")
                .unwrap_or_else(|| panic!("node {id} listed but not found by id"));
            eprintln!("[live] {id} -> {:?}", found.title);
            assert_eq!(found.title, expected.title);
        }

        // And an id KS does not have must be a plain miss, not an error — the
        // JSON 404 read as an answer rather than as a missing route.
        let absent = Uuid::parse_str("ded135c9-0000-4000-8000-000000000000").unwrap();
        assert!(
            client.get_node(absent).await.expect("a 404 is an answer, not a failure").is_none(),
            "an unknown id should be None"
        );
    }

    // GET /nodes is live and confirmed working (it returns the documented
    // {"nodes": [...]} envelope). Kept #[ignore] for the same reason as the
    // transcript check above — it needs a running KS and a real token — not
    // because the route is in doubt.
    #[tokio::test]
    #[ignore = "requires a running Knowledge Store and a real KS_HTTP_TOKEN"]
    async fn live_get_nodes() {
        dotenvy::dotenv().ok();
        dotenvy::from_filename("../.env").ok();

        let client = KsClient::from_env()
            .expect("KS_HTTP_TOKEN must be set in .env to run this test");

        let nodes = client.get_nodes(None).await.expect("GET /nodes should succeed");
        eprintln!("[live] /nodes returned {} node(s)", nodes.len());
        for n in nodes.iter().take(5) {
            eprintln!("[live]   {} | {} | {}", n.id, n.subject, n.title);
        }
    }
}
