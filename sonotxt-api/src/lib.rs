pub mod auth;
pub mod config;
pub mod extractors;
pub mod routes;
pub mod services;

// Re-export core types so existing crate::error / crate::models paths still resolve
pub mod error {
    pub use sonotxt_core::error::*;
}
pub mod models {
    pub use sonotxt_core::models::*;
}

pub use config::Config;
pub use error::{ApiError, Result};

use axum::{http::Method, Router};
use redis::aio::ConnectionManager;
use sqlx::PgPool;
use std::sync::Arc;
use tokio::sync::RwLock;
use tower_http::{
    cors::{Any, CorsLayer},
    limit::RequestBodyLimitLayer,
    timeout::TimeoutLayer,
    trace::TraceLayer,
};

#[derive(Clone)]
pub struct AppState {
    pub config: Config,
    pub redis: ConnectionManager,
    pub http: reqwest::Client,
    pub db: PgPool,
    /// hwpay payment processor with TPM-sealed secrets
    pub payments: Arc<RwLock<hwpay::PaymentProcessor>>,
    /// SONO payment channel service (if configured)
    pub sono: Option<Arc<services::sono::SonoService>>,
    /// GPU worker pool with load balancing and health checks
    pub workers: Option<Arc<services::worker_pool::WorkerPool>>,
}

fn build_cors(origins: &str) -> CorsLayer {
    let cors = CorsLayer::new()
        .allow_methods([Method::GET, Method::POST, Method::PATCH, Method::DELETE])
        .allow_headers(Any);

    if origins.is_empty() {
        cors.allow_origin(Any)
    } else {
        let origins: Vec<_> = origins
            .split(',')
            .filter_map(|s| s.trim().parse().ok())
            .collect();
        cors.allow_origin(origins)
    }
}

pub fn build_app(state: Arc<AppState>) -> Router {
    let cors = build_cors(&state.config.cors_origins);

    Router::new()
        .nest("/api", routes::api::routes())
        .nest("/api", routes::sites::routes())
        .nest("/api", routes::billing::routes())
        .nest("/api", routes::payments::routes())
        .nest("/api", routes::vault::routes())
        .nest("/api/auth", routes::user_auth::routes())
        .merge(routes::auth::routes())
        .merge(routes::admin::routes())
        .merge(routes::ws::routes())
        .merge(routes::embed::routes())
        .merge(routes::audio::routes())
        .nest("/api/voice", routes::converse::routes())
        .nest("/api/sono", routes::sono::routes())
        .nest("/api/auth/passkey", routes::passkey::routes())
        .nest("/api/contacts", routes::contacts::routes())
        .merge(routes::p2p::routes())
        .layer(cors)
        .layer(RequestBodyLimitLayer::new(10 * 1024 * 1024))
        .layer(TimeoutLayer::new(std::time::Duration::from_secs(
            state.config.request_timeout,
        )))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// Notify workers of a new job. Pushes via QUIC (immediate) + redis (fallback).
pub async fn notify_job(state: &Arc<AppState>, job_id: &str) {
    // QUIC push: instant wake-up for connected workers
    if let Some(ref pool) = state.workers {
        pool.notify_job(job_id).await;
    }

    // Redis pub/sub: catches workers not yet QUIC-connected
    let result: std::result::Result<(), _> = redis::cmd("PUBLISH")
        .arg("job:notify")
        .arg(job_id)
        .query_async(&mut state.redis.clone())
        .await;
    if let Err(e) = result {
        tracing::warn!("redis job:notify failed: {}", e);
    }
}
