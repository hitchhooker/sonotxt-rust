use axum::{
    extract::State,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::{
    auth::TtsUser,
    error::{ApiError, Result},
    services::billing,
    AppState,
};

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/billing/status", get(get_status))
        .route("/billing/estimate", post(estimate_cost))
        .route("/billing/withdraw", post(withdraw_txt))
}

#[derive(Debug, Serialize)]
struct BalanceStatus {
    /// Custodial TXT balance (raw units)
    txt_balance: i64,
    /// Custodial balance formatted
    txt_formatted: String,
    /// Payment channel balance (raw units, 0 if no channel)
    channel_balance: i64,
    /// Channel balance formatted
    channel_formatted: String,
    /// Total available TXT
    total: i64,
    total_formatted: String,
    /// Current TXT price in USD
    txt_usd: f64,
    /// Total value in USD
    total_usd: f64,
}

async fn get_status(
    State(state): State<Arc<AppState>>,
    user: TtsUser,
) -> Result<Json<BalanceStatus>> {
    let (user_id, wallet_addr) = match &user {
        TtsUser::Authenticated(u) => (u.account_id, u.wallet_address.as_deref()),
        TtsUser::FreeTier { .. } => {
            return Ok(Json(BalanceStatus {
                txt_balance: 0,
                txt_formatted: "0".into(),
                channel_balance: 0,
                channel_formatted: "0".into(),
                total: 0,
                total_formatted: "0".into(),
                txt_usd: 0.01,
                total_usd: 0.0,
            }));
        }
    };

    let (custodial, channel) = billing::total_balance(
        &state.db,
        state.sono.as_deref(),
        user_id,
        wallet_addr,
    ).await?;

    let total = custodial.saturating_add(channel);
    let txt_usd = state.sono.as_ref()
        .map(|s| {
            // Use try_read to avoid blocking, fall back to default
            s.price.try_read().map(|p| p.txt_usd_base).unwrap_or(0.01)
        })
        .unwrap_or(0.01);

    let total_usd = total as f64 / billing::TXT_DECIMALS as f64 * txt_usd;

    Ok(Json(BalanceStatus {
        txt_balance: custodial,
        txt_formatted: billing::format_txt(custodial),
        channel_balance: channel,
        channel_formatted: billing::format_txt(channel),
        total,
        total_formatted: billing::format_txt(total),
        txt_usd,
        total_usd,
    }))
}

#[derive(Debug, Deserialize)]
struct EstimateRequest {
    chars: usize,
}

#[derive(Debug, Serialize)]
struct EstimateResponse {
    /// TXT cost in raw units
    txt_cost: i64,
    txt_formatted: String,
    /// Equivalent USD cost
    usd_cost: f64,
}

async fn estimate_cost(
    State(state): State<Arc<AppState>>,
    Json(req): Json<EstimateRequest>,
) -> Result<Json<EstimateResponse>> {
    let price = match &state.sono {
        Some(sono) => sono.price.read().await.clone(),
        None => crate::services::sono::PriceInfo::default(),
    };

    let txt_cost = billing::txt_cost_for_chars(req.chars, state.config.cost_per_char, &price);
    let usd_cost = req.chars as f64 * state.config.cost_per_char;

    Ok(Json(EstimateResponse {
        txt_cost,
        txt_formatted: billing::format_txt(txt_cost),
        usd_cost,
    }))
}

#[derive(Debug, Deserialize)]
struct WithdrawRequest {
    /// TXT amount in raw units
    amount: i64,
    /// Destination SS58 wallet address
    wallet_address: String,
}

#[derive(Debug, Serialize)]
struct WithdrawResponse {
    tx_hash: String,
    amount: i64,
    amount_formatted: String,
    new_balance: i64,
}

/// POST /billing/withdraw — withdraw custodial TXT to an on-chain wallet
async fn withdraw_txt(
    State(state): State<Arc<AppState>>,
    user: crate::auth::AuthenticatedUser,
    Json(req): Json<WithdrawRequest>,
) -> Result<Json<WithdrawResponse>> {
    if req.amount <= 0 {
        return Err(ApiError::InvalidRequest("amount must be positive".into()));
    }

    let sono = state.sono.as_ref()
        .ok_or_else(|| ApiError::InvalidRequest("withdrawals not available".into()))?;

    // Resolve destination EVM address
    let evm_addr = crate::routes::sono::ss58_to_h160(&req.wallet_address)
        .map_err(|e| ApiError::InvalidRequest(format!("invalid wallet address: {}", e)))?;

    // Deduct from custodial balance atomically
    let new_balance: i64 = sqlx::query_scalar(
        "UPDATE users SET txt_balance = txt_balance - $1 WHERE id = $2 AND txt_balance >= $1 RETURNING txt_balance",
    )
    .bind(req.amount)
    .bind(user.account_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or(ApiError::InsufficientBalance)?;

    // Send TXT on-chain via contract transfer
    let txt_u256 = alloy::primitives::U256::from(req.amount as u128);
    match sono.drip_testnet(evm_addr, alloy::primitives::U256::ZERO, txt_u256).await {
        Ok(()) => {
            tracing::info!(
                user = %user.account_id, to = %evm_addr, amount = req.amount,
                "TXT withdrawal sent"
            );
            Ok(Json(WithdrawResponse {
                tx_hash: format!("pending"), // drip_testnet doesn't return hash yet
                amount: req.amount,
                amount_formatted: billing::format_txt(req.amount),
                new_balance,
            }))
        }
        Err(e) => {
            // Rollback: re-credit the balance
            let _ = sqlx::query(
                "UPDATE users SET txt_balance = txt_balance + $1 WHERE id = $2",
            )
            .bind(req.amount)
            .bind(user.account_id)
            .execute(&state.db)
            .await;

            tracing::error!(user = %user.account_id, "TXT withdrawal failed: {}", e);
            Err(ApiError::Internal(format!("withdrawal failed: {}", e)))
        }
    }
}
