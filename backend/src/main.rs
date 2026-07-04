use actix_web::{get, App, HttpServer};

#[get("/health")]
async fn health() -> &'static str {
    "ok"
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    HttpServer::new(|| App::new().service(health))
        .bind(("127.0.0.1", 8081))?
        .run()
        .await
}
