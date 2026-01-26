pub mod auth;
pub mod config;
pub mod error;
pub mod extractors;
pub mod models;
pub mod routes;
pub mod services;
pub mod worker;

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
}

fn build_cors(origins: &str) -> CorsLayer {
    let cors = CorsLayer::new()
        .allow_methods([Method::GET, Method::POST, Method::PATCH, Method::DELETE])
        .allow_headers(Any);

    if origins.is_empty() {
        // Development: allow all origins
        cors.allow_origin(Any)
    } else {
        // Production: parse comma-separated origins
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
        .layer(cors)
        .layer(RequestBodyLimitLayer::new(1024 * 1024))
        .layer(TimeoutLayer::new(std::time::Duration::from_secs(
            state.config.request_timeout,
        )))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}
