use sonotxt::{build_app, worker, AppState, Config};
use std::sync::Arc;
use tokio::sync::RwLock;

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

    sqlx::migrate!("./migrations")
        .run(&db)
        .await
        .expect("Failed to run migrations");

    let http = reqwest::Client::builder()
        .user_agent("SonoTxt/1.0")
        .timeout(std::time::Duration::from_secs(config.request_timeout))
        .build()
        .expect("HTTP client failed");

    // Initialize hwpay vault (TPM-first, falls back to encrypted file)
    // Password from env for encrypted fallback when TPM unavailable
    let vault_password = std::env::var("VAULT_PASSWORD").ok();
    let vault = hwpay::Vault::open(vault_password.as_deref().map(|s| s.as_bytes()))
        .expect("Failed to open vault");
    let payments = hwpay::PaymentProcessor::new(vault);

    let state = Arc::new(AppState {
        config: config.clone(),
        redis,
        http,
        db,
        payments: Arc::new(RwLock::new(payments)),
    });

    // Spawn TTS worker
    let worker_state = state.clone();
    tokio::spawn(async move {
        worker::run_worker(worker_state).await;
    });

    let app = build_app(state);

    let addr = format!("{}:{}", config.host, config.port);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .expect("Bind failed");
    
    tracing::info!("Server running on {}", addr);
    
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>()
    )
    .await
    .expect("Server failed");
}
