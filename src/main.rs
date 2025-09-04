mod auth;
mod config;
mod error;
mod routes;
mod services;
mod worker;

use axum::Router;
use sqlx::postgres::PgPoolOptions;
use std::sync::Arc;

pub struct AppState {
    pub db: sqlx::PgPool,
    pub config: config::Config,
}

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();
    env_logger::init();

    let config = config::Config::from_env();
    let db = PgPoolOptions::new()
        .max_connections(5)
        .connect(&config.database_url)
        .await
        .expect("Failed to connect to database");

    sqlx::migrate!("./migrations")
        .run(&db)
        .await
        .expect("Failed to run migrations");

    let state = Arc::new(AppState { db, config });

    // Spawn worker task
    let worker_state = state.clone();
    tokio::spawn(async move {
        crate::worker::run_worker(worker_state).await;
    });

    let app = Router::new()
        .nest("/", routes::api::routes())
        .with_state(state);

    println!("Server starting on http://0.0.0.0:8080");
    
    let listener = tokio::net::TcpListener::bind("0.0.0.0:8080")
        .await
        .expect("Failed to bind");

    axum::serve(listener, app)
        .await
        .expect("Server failed");
}
