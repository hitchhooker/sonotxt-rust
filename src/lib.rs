pub mod auth;
pub mod config;
pub mod error;
pub mod models;
pub mod routes;
pub mod extractors;
pub mod services;

pub use config::Config;
pub use error::{ApiError, Result};

use axum::Router;
use redis::aio::ConnectionManager;
use sqlx::PgPool;
use std::sync::Arc;
use tower_http::{
    cors::CorsLayer,
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
}

pub fn build_app(state: Arc<AppState>) -> Router {
    Router::new()
        .merge(routes::api::routes())
        .merge(routes::admin::routes())
        .merge(routes::sites::routes())
        .merge(routes::billing::routes())
        .layer(CorsLayer::very_permissive())
        .layer(RequestBodyLimitLayer::new(1024 * 1024))
        .layer(TimeoutLayer::new(std::time::Duration::from_secs(
            state.config.request_timeout,
        )))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}
