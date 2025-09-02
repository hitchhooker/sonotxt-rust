use axum::{
    extract::State,
    routing::post,
    Json, Router,
};
use axum_extra::{
    headers::{authorization::Bearer, Authorization},
    TypedHeader,
};
use std::sync::Arc;
use uuid::Uuid;

use crate::{models::ApiKey, AppState, Result};

pub fn routes() -> Router<Arc<AppState>> {
    Router::new().route("/admin/apikey", post(create_api_key))
}

async fn create_api_key(
    State(state): State<Arc<AppState>>,
    TypedHeader(auth): TypedHeader<Authorization<Bearer>>,
    Json(req): Json<serde_json::Value>,
) -> Result<Json<ApiKey>> {
    match &state.config.admin_token {
        Some(token) if token == auth.token() => {},
        _ => return Err(crate::error::ApiError::Unauthorized),
    }
    
    let balance = req["balance"].as_f64().unwrap_or(10.0);
    let account_id = Uuid::new_v4(); // Create account too in real version
    let api_key = ApiKey::new(account_id, balance);
    
    let mut redis = state.redis.clone();
    redis::cmd("SET")
        .arg(format!("apikey:{}", api_key.key))
        .arg(serde_json::to_string(&api_key).unwrap())
        .query_async::<_, ()>(&mut redis)
        .await
        .map_err(|_| crate::error::ApiError::Internal)?;
    
    Ok(Json(api_key))
}
