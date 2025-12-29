// src/routes/billing.rs
use axum::{
    extract::{State, Query},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::{
    auth::AuthenticatedUser,
    error::Result,
    services::billing,
    AppState,
};

#[derive(Deserialize)]
struct PurchaseCreditsRequest {
    amount: f64, // $5, $10, $25, $50
}

#[derive(Serialize)]
struct AccountStatus {
    balance: f64,
    subscription_type: Option<String>,
    subscription_expires: Option<chrono::DateTime<chrono::Utc>>,
    watermark_free: bool,
}

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/billing/status", get(get_status))
        .route("/billing/estimate", post(estimate_cost))
        .route("/billing/purchase", post(purchase_credits))
        .route("/billing/subscribe", post(subscribe))
}

async fn get_status(
    State(state): State<Arc<AppState>>,
    user: AuthenticatedUser,
) -> Result<Json<AccountStatus>> {
    let account = sqlx::query!(
        r#"
        SELECT balance, subscription_type, subscription_expires, watermark_free
        FROM account_credits
        WHERE account_id = $1
        "#,
        user.account_id
    )
    .fetch_optional(&state.db)
    .await?;
    
    let status = match account {
        Some(acc) => AccountStatus {
            balance: acc.balance,
            subscription_type: acc.subscription_type,
            subscription_expires: acc.subscription_expires,
            watermark_free: acc.watermark_free,
        },
        None => {
            // Create account with free credits
            sqlx::query!(
                "INSERT INTO account_credits (account_id) VALUES ($1)",
                user.account_id
            )
            .execute(&state.db)
            .await?;
            
            AccountStatus {
                balance: 5.0,
                subscription_type: None,
                subscription_expires: None,
                watermark_free: false,
            }
        }
    };
    
    Ok(Json(status))
}

async fn estimate_cost(
    State(state): State<Arc<AppState>>,
    user: AuthenticatedUser,
    Json(request): Json<billing::TtsRequest>,
) -> Result<Json<billing::PricingEstimate>> {
    let estimate = billing::estimate_cost(&state, user.account_id, &request).await?;
    Ok(Json(estimate))
}

async fn purchase_credits(
    State(state): State<Arc<AppState>>,
    user: AuthenticatedUser,
    Json(req): Json<PurchaseCreditsRequest>,
) -> Result<Json<serde_json::Value>> {
    // In production, integrate with Stripe here
    // For now, just add credits directly

    if !vec![5.0, 10.0, 25.0, 50.0].contains(&req.amount) {
        return Err(crate::error::ApiError::InvalidRequestError);
    }

    let new_balance = billing::add_credits(&state, user.account_id, req.amount, None).await?;
    
    Ok(Json(serde_json::json!({
        "success": true,
        "new_balance": new_balance
    })))
}

async fn subscribe(
    State(state): State<Arc<AppState>>,
    user: AuthenticatedUser,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Result<Json<serde_json::Value>> {
    let plan = params.get("plan")
        .ok_or(crate::error::ApiError::InvalidRequestError)?;

    // In production, create Stripe subscription
    // For now, activate directly
    billing::activate_subscription(
        &state,
        user.account_id,
        plan,
        "sub_test_123".to_string()
    ).await?;
    
    Ok(Json(serde_json::json!({
        "success": true,
        "plan": plan,
        "message": "Subscription activated"
    })))
}
