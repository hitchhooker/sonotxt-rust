use sonotxt::{build_app, AppState, Config};
use std::sync::Arc;

#[tokio::main]
async fn main() {
    let config = Config::from_env();
    
    tracing_subscriber::fmt()
        .with_env_filter(&config.log_level)
        .init();
    
    let redis_client = redis::Client::open(config.redis_url.as_str())
        .expect("Redis connection failed");
    
    let redis = redis::aio::ConnectionManager::new(redis_client)
        .await
        .expect("Redis manager failed");
    
    let db = sqlx::postgres::PgPoolOptions::new()
        .max_connections(5)
        .connect(&config.database_url)
        .await
        .expect("Database connection failed");
    
    // Skip migrations - tables already exist
    // sqlx::migrate!("./migrations")
    //     .run(&db)
    //     .await
    //     .expect("Failed to run migrations");
    
    let http = reqwest::Client::builder()
        .user_agent("SonoTxt/1.0")
        .timeout(std::time::Duration::from_secs(config.request_timeout))
        .build()
        .expect("HTTP client failed");
    
    let state = Arc::new(AppState {
        config: config.clone(),
        redis,
        http,
        db,
    });
    
    let app = build_app(state);
    
    let addr = format!("{}:{}", config.host, config.port);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .expect("Bind failed");
    
    tracing::info!("Server running on {}", addr);
    
    axum::serve(listener, app)
        .await
        .expect("Server failed");
}
