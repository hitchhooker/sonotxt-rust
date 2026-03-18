use sonotxt_api::{build_app, AppState, Config};
use sonotxt_api::services::payments::assethub::{AssetHubListener, DepositHandler};
use sonotxt_api::services::payments::penumbra::PenumbraListener;
use sonotxt_api::services::sono::{SonoConfig, SonoService};
use sonotxt_api::services::worker_pool::WorkerPool;
use std::sync::Arc;
use tokio::sync::RwLock;

#[tokio::main]
async fn main() {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

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

    sqlx::migrate!("../migrations")
        .run(&db)
        .await
        .expect("Failed to run migrations");

    let http = reqwest::Client::builder()
        .user_agent("sonotxt/1.0")
        .timeout(std::time::Duration::from_secs(config.request_timeout))
        .build()
        .expect("HTTP client failed");

    // Initialize hwpay vault (TPM-first, falls back to encrypted file)
    let vault_password = std::env::var("VAULT_PASSWORD").ok();
    let vault = hwpay::Vault::open(vault_password.as_deref().map(|s| s.as_bytes()))
        .expect("Failed to open vault");
    let payments = hwpay::PaymentProcessor::new(vault);

    // Initialize SONO payment channel service (if configured)
    let sono = SonoConfig::from_env().map(|cfg| {
        tracing::info!(
            contract = %cfg.contract,
            service = %cfg.service_key.address(),
            "SONO payment channels enabled"
        );
        Arc::new(SonoService::new(cfg))
    });

    // Initialize worker pool (if WORKER_URLS configured)
    // Connects QUIC+Noise to each worker for encrypted transport.
    let workers = match config.worker_urls.as_ref() {
        Some(urls) => Some(Arc::new(WorkerPool::new(urls, http.clone()).await)),
        None => None,
    };

    let state = Arc::new(AppState {
        config: config.clone(),
        redis,
        http,
        db,
        payments: Arc::new(RwLock::new(payments)),
        sono,
        workers,
    });

    // Spawn worker pool health checker (every 10s)
    if let Some(ref pool) = state.workers {
        let pool_len = pool.len();
        let pool_bg = pool.clone();
        tokio::spawn(async move {
            loop {
                pool_bg.health_check().await;
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            }
        });
        tracing::info!("worker pool health checker started ({} workers)", pool_len);
    }

    // Spawn TTS job processor — polls DB queue, routes through worker pool
    if state.workers.is_some() {
        let job_state = state.clone();
        tokio::spawn(async move {
            sonotxt_api::job_worker::run(job_state).await;
        });
        tracing::info!("TTS job worker started");
    }

    // Spawn Asset Hub deposit listener (if enabled)
    if config.assethub_listener_enabled {
        let listener_state = state.clone();
        tokio::spawn(async move {
            if let Err(e) = spawn_assethub_listener(listener_state).await {
                tracing::error!("assethub listener failed: {}", e);
            }
        });
    }

    // Spawn Penumbra deposit listener (if rpc configured)
    if config.penumbra_rpc.is_some() {
        let listener_state = state.clone();
        tokio::spawn(async move {
            let listener = PenumbraListener::new(listener_state);
            if let Err(e) = listener.run().await {
                tracing::error!("penumbra listener failed: {}", e);
            }
        });
    }

    // Spawn SONO payment channel listener + price oracle (if configured)
    if let Some(sono) = &state.sono {
        let sono_listener = sono.clone();
        tokio::spawn(async move {
            if let Err(e) = sono_listener.start_listener().await {
                tracing::error!("SONO listener failed: {}", e);
            }
        });

        let sono_oracle = sono.clone();
        let oracle_http = state.http.clone();
        tokio::spawn(async move {
            if let Err(e) = sono_oracle.start_price_oracle(oracle_http).await {
                tracing::error!("SONO price oracle failed: {}", e);
            }
        });
    }

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

async fn spawn_assethub_listener(state: Arc<AppState>) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tracing::info!("initializing assethub deposit listener");

    let wallet = if let Some(seed_hex) = &state.config.deposit_wallet_seed {
        let seed_bytes = hex::decode(seed_hex)
            .map_err(|e| format!("invalid hex seed: {}", e))?;
        hwpay::PolkadotWallet::from_seed(&seed_bytes)
            .map_err(|e| format!("invalid wallet seed: {}", e))?
    } else {
        let mut payments = state.payments.write().await;
        let vault = payments.vault_mut();
        hwpay::PolkadotWallet::from_vault(vault)
            .map_err(|e| format!("no wallet seed configured: {}", e))?
    };

    let handler = if let Some(medium_wallet) = &state.config.assethub_medium_wallet {
        tracing::info!("auto-sweep enabled to {}", medium_wallet);

        let mut sweeper = hwpay::Sweeper::new(&state.config.assethub_rpc);
        sweeper.connect().await
            .map_err(|e| format!("sweeper connect failed: {}", e))?;

        DepositHandler::with_sweep(
            state.db.clone(),
            sweeper,
            wallet,
            medium_wallet.clone(),
            state.config.assethub_usdc_asset_id,
            state.config.assethub_usdt_asset_id,
        )
    } else {
        tracing::info!("auto-sweep disabled (no medium wallet configured)");
        DepositHandler::new(state.db.clone())
    };

    let mut listener = AssetHubListener::new(state);
    listener.run_with_handler(handler).await
        .map_err(|e| format!("listener error: {}", e))?;

    Ok(())
}
