mod config;
mod health;
mod processor;
mod quic;

use config::WorkerConfig;
use processor::WorkerState;
use std::sync::Arc;
use tracing::{error, info};

#[tokio::main]
async fn main() {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    let config = WorkerConfig::from_env();

    tracing_subscriber::fmt()
        .with_env_filter(&config.log_level)
        .init();

    let http = reqwest::Client::builder()
        .user_agent("sonotxt-worker/1.0")
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .expect("HTTP client failed");

    let speech_url = config.speech_url.clone();
    let llm_url = config.llm_url.clone();
    let health_port = config.health_port;
    let quic_port = config.quic_port;

    let state = Arc::new(WorkerState {
        config,
        http,
    });

    // Spawn HTTP health server (for legacy monitoring / vast.ai health checks)
    tokio::spawn(async move {
        let app = health::health_router(speech_url, llm_url);
        let addr = format!("0.0.0.0:{}", health_port);
        let listener = tokio::net::TcpListener::bind(&addr)
            .await
            .expect("Health server bind failed");
        info!("HTTP health server on :{}", health_port);
        axum::serve(listener, app).await.expect("Health server failed");
    });

    // QUIC server — the only external interface.
    // API connects here, sends encrypted TTS requests, gets encrypted audio back.
    // No DB, no Redis — all job dispatch happens over this channel.
    let addr: std::net::SocketAddr = format!("0.0.0.0:{}", quic_port).parse().unwrap();
    info!("sonotxt-worker starting (QUIC :{}, health :{})", quic_port, health_port);

    match quic::QuicWorkerServer::new(state) {
        Ok(server) => {
            if let Err(e) = server.run(addr).await {
                error!("QUIC server error: {:?}", e);
            }
        }
        Err(e) => error!("QUIC server init failed: {:?}", e),
    }
}
