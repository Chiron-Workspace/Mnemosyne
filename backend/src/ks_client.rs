//! HTTP client for the Chiron Knowledge Store (KS).
//!
//! KS is a **separate Python service** (Postgres-backed) reachable over
//! localhost HTTP. It is not callable in-process from this Rust backend — the
//! two live in different runtimes — so every interaction goes through this
//! client.
//!
//! Scope is deliberately narrow: `GET /health`, `POST /transcripts`, and
//! `GET /nodes`. `POST /ingest` is **not** implemented here. Per the agreed
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

/// Default KS port. Overridable via `KS_HTTP_PORT`.
const DEFAULT_PORT: &str = "8080";

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
/// **The exact response shape is not yet fixed.** At the time this was
/// written, KS exposed only `/health`, `/transcripts` and `/ingest` — the
/// `/nodes` route did not exist, so there was no contract to code against.
/// Rather than guess once and be wrong, this accepts the two shapes KS
/// plausibly returns: an envelope `{"nodes": [...]}` (matching how
/// `/transcripts` wraps its result) or a bare array `[...]`. When the real
/// route lands, confirm which one it is and this can be narrowed.
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
    /// **Not yet exercised against a live KS.** The `/nodes` route did not
    /// exist when this was written; see [`parse_nodes_response`] for the shape
    /// assumption this makes.
    pub async fn get_nodes(&self, subject: Option<&str>) -> Result<Vec<KsNode>, KsError> {
        let url = match subject {
            Some(subject) => format!(
                "{}/nodes?subject={}",
                self.base_url,
                percent_encode_query_value(subject)
            ),
            None => format!("{}/nodes", self.base_url),
        };

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

    // Separate from the transcript live check because it depends on a KS route
    // that does not exist yet. Un-ignore only once GET /nodes is confirmed
    // live, then verify with log correlation the same way /transcripts was.
    #[tokio::test]
    #[ignore = "GET /nodes does not exist in KS yet — un-ignore once the route is confirmed live"]
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
