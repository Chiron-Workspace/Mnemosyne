use actix_web::{get, web, App, HttpServer, HttpResponse};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

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
    // Load .env at the very start, before reading any env vars.
    dotenvy::dotenv().ok();

    let database_url = std::env::var("DATABASE_URL").unwrap_or_else(|_| {
        panic!("DATABASE_URL is not set. Add it to .env (see .env.example).");
    });

    // Create a PostgreSQL connection pool. Max 5 connections — this is a
    // 2-3 user app, no need for a large pool.
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&database_url)
        .await
        .unwrap_or_else(|e| {
            panic!("Failed to connect to database: {e}");
        });

    HttpServer::new(move || {
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .service(health)
            .service(health_db)
    })
    .bind(("127.0.0.1", 8081))?
    .run()
    .await
}
