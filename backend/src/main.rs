use actix_web::{get, web, App, HttpServer, HttpResponse};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

use mnemosyne_core::scheduling::FsrsScheduler;

mod deepseek;
mod handlers;
mod ks_client;
mod llm_provider;

#[get("/health")]
async fn health() -> &'static str {
    "ok"
}

/// Database health check: runs a real query (`SELECT COUNT(*) FROM users`)
/// against Postgres and reports the result. Returns 200 with the count on
/// success, 500 with the error message on failure.
#[get("/health/db")]
async fn health_db(pool: web::Data<PgPool>) -> HttpResponse {
    match sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM users")
        .fetch_one(pool.get_ref())
        .await
    {
        Ok(count) => HttpResponse::Ok().json(serde_json::json!({
            "status": "ok",
            "user_count": count,
        })),
        Err(e) => HttpResponse::InternalServerError().json(serde_json::json!({
            "status": "error",
            "message": e.to_string(),
        })),
    }
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    eprintln!("[mnemosyne] starting up...");
    // Load .env at the very start, before reading any env vars.
    dotenvy::dotenv().ok();
    eprintln!("[mnemosyne] .env loaded");

    let database_url = std::env::var("DATABASE_URL").unwrap_or_else(|_| {
        panic!("DATABASE_URL is not set. Add it to .env (see .env.example).");
    });

    // Create a PostgreSQL connection pool. Max 5 connections — this is a
    // 2-3 user app, no need for a large pool.
    //
    // Mnemosyne talks to the local Postgres cluster directly (no connection
    // pooler in front of it), so sqlx's default prepared-statement caching is
    // fine and no PgBouncer-style workaround is needed.
    eprintln!("[mnemosyne] connecting to DB...");
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&database_url)
        .await
        .unwrap_or_else(|e| {
            panic!("Failed to connect to database: {e}");
        });
    eprintln!("[mnemosyne] DB pool ready");

    // Construct the FSRS scheduler once and share it across all workers via
    // web::Data (which is Arc internally; no Clone needed on the scheduler).
    let scheduler = web::Data::new(FsrsScheduler::default());

    // Construct the LLM provider via env-configured selection. Defaults to
    // "deepseek" to preserve the pre-refactor startup behavior. Fails fast at
    // startup if the selected provider's required API key is missing — silent
    // absence of an AI subsystem is worse than a clear panic.
    let provider_name = std::env::var("LLM_PROVIDER").unwrap_or_else(|_| "deepseek".to_string());
    let llm_provider: Box<dyn llm_provider::LLMProvider> = match provider_name.as_str() {
        "deepseek" => Box::new(
            deepseek::DeepSeekClient::from_env().unwrap_or_else(|| {
                panic!("DEEPSEEK_API_KEY is not set in .env. Add it (see .env.example).");
            })
        ),
        other => panic!("Unknown LLM_PROVIDER: {other}"),
    };
    eprintln!("[mnemosyne] LLM provider ready ({provider_name})");
    let llm_provider = web::Data::new(llm_provider);

    // Knowledge Store client. Deliberately NOT fail-fast: unlike the LLM
    // provider, KS is auxiliary bookkeeping, so a missing KS_HTTP_TOKEN warns
    // once here and disables transcript sync — a study session must still run.
    let ks_client = ks_client::KsClient::from_env();
    match &ks_client {
        Some(client) => {
            eprintln!("[mnemosyne] Knowledge Store client ready");
            // Probe /health once at startup. It needs no auth, so its result
            // separates the two failure modes that otherwise look alike later:
            // a failure here means the KS process is down, whereas a healthy
            // probe followed by a 403 on /transcripts means the token is wrong.
            // Purely diagnostic — a down KS never blocks startup.
            match client.health().await {
                Ok(()) => eprintln!("[mnemosyne] Knowledge Store /health: ok"),
                Err(e) => eprintln!(
                    "[mnemosyne] WARNING: Knowledge Store /health probe failed: {e} \
                     — transcript sync will be attempted anyway and logged per session."
                ),
            }
        }
        None => eprintln!(
            "[mnemosyne] WARNING: KS_HTTP_TOKEN is not set — transcript sync to the \
             Knowledge Store is DISABLED. Study sessions are unaffected. \
             Set KS_HTTP_TOKEN in .env to enable it (see .env.example)."
        ),
    }
    let ks_client = web::Data::new(ks_client);

    // No CORS layer: this API has no browser-based consumer. Sessions are
    // driven by direct HTTP calls (coding agents, curl), and CORS only
    // constrains requests originating from a web page. If a Chiron OS shell
    // ever calls this backend from a browser, add actix-cors back and
    // configure it against that shell's actual origin rather than a wildcard.
    HttpServer::new(move || {
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(scheduler.clone())
            .app_data(llm_provider.clone())
            .app_data(ks_client.clone())
            .service(health)
            .service(health_db)
            .service(handlers::users::create_user)
            .service(handlers::users::list_users)
            .service(handlers::study_sets::create_study_set)
            .service(handlers::study_sets::list_study_sets)
            .service(handlers::cards::create_card)
            .service(handlers::cards::list_cards)
            .service(handlers::reviews::review)
            .service(handlers::generate::generate_cards)
            .service(handlers::due::due)
            .service(handlers::socratic::start)
            .service(handlers::socratic::reply)
            .service(handlers::socratic::end)
            .service(handlers::socratic::get_session)
            .service(handlers::feynman::evaluate)
            .service(handlers::feynman::history)
            .service(handlers::quiz::generate_quiz)
            .service(handlers::quiz::attempt)
            .service(handlers::quiz::list_questions)
    })
    .bind(("127.0.0.1", 8081))?
    .run()
    .await
    .inspect_err(|e| eprintln!("[mnemosyne] server stopped: {e}"))
}