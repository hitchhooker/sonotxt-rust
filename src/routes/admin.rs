use axum::{
    extract::State,
    routing::post,
    Json, Router,
};
use axum_extra::{
    headers::{authorization::Bearer, Authorization},
    TypedHeader,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use subtle::ConstantTimeEq;
use uuid::Uuid;

use crate::{auth::api_key::ApiKey, error::Result, AppState};

#[derive(Debug, Serialize)]
struct CreateApiKeyResponse {
    key: String,
    account_id: Uuid,
    balance: f64,
    created_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
struct CreateApiKeyRequest {
    #[serde(default = "default_balance")]
    balance: f64,
    #[serde(default)]
    account_id: Option<Uuid>,
}

fn default_balance() -> f64 {
    10.0
}

pub fn routes() -> Router<Arc<AppState>> {
    Router::new().route("/admin/apikey", post(create_api_key))
}

async fn create_api_key(
    State(state): State<Arc<AppState>>,
    TypedHeader(auth): TypedHeader<Authorization<Bearer>>,
    Json(req): Json<CreateApiKeyRequest>,
) -> Result<Json<CreateApiKeyResponse>> {
    // Constant-time comparison to prevent timing attacks
    let is_valid = match &state.config.admin_token {
        Some(token) => {
            let a = token.as_bytes();
            let b = auth.token().as_bytes();
            a.len() == b.len() && a.ct_eq(b).into()
        }
        None => false,
    };

    if !is_valid {
        return Err(crate::error::ApiError::Unauthorized);
    }

    let account_id = req.account_id.unwrap_or_else(Uuid::new_v4);
    let api_key = Uuid::new_v4().to_string();
    let now = Utc::now();

    let mut tx = state.db.begin().await?;

    // Create account if it doesn't exist
    sqlx::query!(
        r#"
        INSERT INTO accounts (id)
        VALUES ($1)
        ON CONFLICT (id) DO NOTHING
        "#,
        account_id
    )
    .execute(&mut *tx)
    .await?;

    // Create account_credits if it doesn't exist
    sqlx::query!(
        r#"
        INSERT INTO account_credits (account_id, balance)
        VALUES ($1, $2)
        ON CONFLICT (account_id) DO UPDATE SET balance = account_credits.balance + $2
        "#,
        account_id,
        req.balance
    )
    .execute(&mut *tx)
    .await?;

    // Create api_key in database
    sqlx::query!(
        r#"
        INSERT INTO api_keys (key, account_id, created_at)
        VALUES ($1, $2, $3)
        "#,
        api_key,
        account_id,
        now
    )
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    // Cache in Redis
    let key_data = ApiKey {
        key: api_key.clone(),
        account_id,
        created_at: now,
        revoked: false,
    };

    let mut redis = state.redis.clone();
    let _ = redis::cmd("SETEX")
        .arg(format!("apikey:{}", api_key))
        .arg(300)
        .arg(serde_json::to_string(&key_data).unwrap_or_default())
        .query_async::<_, ()>(&mut redis)
        .await;

    Ok(Json(CreateApiKeyResponse {
        key: api_key,
        account_id,
        balance: req.balance,
        created_at: now,
    }))
}
