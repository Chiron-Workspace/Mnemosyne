use actix_web::{get, web, App, HttpServer, HttpResponse};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::PgPool;

use mnemosyne_core::scheduling::FsrsScheduler;

mod deepseek;
mod handlers;

#[get("/health")]
async fn health() -> &'static str {
    "ok"
}

/// Database health check: runs a real query (`SELECT COUNT(*) FROM users`)
/// against Supabase and reports the result. Returns 200 with the count on
/// success, 500 with the error message on failure.
#[get("/health/db")]
async fn health_db(pool: web::Data<PgPool>) -> HttpResponse {
    match sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM users")
        .persistent(false)
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
    // We use connect_with() instead of connect(&url) so we can set
    // statement_cache_capacity(0) on PgConnectOptions. The Supabase pooler
    // (PgBouncer in transaction mode, port 6543) does not support persistent
    // prepared statements across transactions, and re-using a cached statement
    // name on a different pooled backend connection raises
    // "prepared statement \"sqlx_s_N\" already exists". Disabling the cache
    // is the documented workaround for sqlx + PgBouncer.
    let connect_options: PgConnectOptions = database_url
        .parse()
        .expect("DATABASE_URL is not a valid PostgreSQL connection string");
    eprintln!("[mnemosyne] connecting to DB...");
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect_with(connect_options.statement_cache_capacity(0))
        .await
        .unwrap_or_else(|e| {
            panic!("Failed to connect to database: {e}");
        });
    eprintln!("[mnemosyne] DB pool ready");

    // Construct the FSRS scheduler once and share it across all workers via
    // web::Data (which is Arc internally; no Clone needed on the scheduler).
    let scheduler = web::Data::new(FsrsScheduler::default());

    // Construct the DeepSeek HTTP client once and share it the same way. Fails
    // fast at startup if the API key is missing — silent absence of an AI
    // subsystem is worse than a clear panic.
    let deepseek_client = deepseek::DeepSeekClient::from_env().unwrap_or_else(|| {
        panic!("DEEPSEEK_API_KEY is not set in .env. Add it (see .env.example).");
    });
    eprintln!("[mnemosyne] DeepSeek client ready");
    let deepseek = web::Data::new(deepseek_client);

    HttpServer::new(move || {
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(scheduler.clone())
            .app_data(deepseek.clone())
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
            .service(handlers::socratic::get_session)
    })
    .bind(("127.0.0.1", 8081))?
    .run()
    .await
    .inspect_err(|e| eprintln!("[mnemosyne] server stopped: {e}"))
}