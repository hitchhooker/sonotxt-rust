//! SONO payment channel API routes
//!
//! GET  /api/sono/channel?user=0x...  — channel status for a user
//! GET  /api/sono/balance?user=0x...  — SONO balance check
//! POST /api/sono/sign-state          — get service-signed state update
//! POST /api/sono/settle              — cooperatively close channel

use axum::{
    extract::{Query, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::error;

use alloy::primitives::Address;

use crate::AppState;

/// Convert an SS58 address to an EVM H160 address.
/// pallet-revive maps AccountId32 → H160 by taking the first 20 bytes.
pub fn ss58_to_h160(ss58: &str) -> Result<Address, String> {
    let pubkey = crate::services::user_auth::decode_ss58(ss58)?;
    let mut h160 = [0u8; 20];
    h160.copy_from_slice(&pubkey[..20]);
    Ok(Address::from(h160))
}

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/channel", get(channel_status))
        .route("/balance", get(balance))
        .route("/sign-state", post(sign_state))
        .route("/settle", post(settle))
        .route("/info", get(info))
        .route("/price", get(price))
}

#[derive(Debug, Deserialize)]
struct UserQuery {
    user: String,
}

#[derive(Debug, Serialize)]
struct ChannelResponse {
    has_channel: bool,
    deposit: String,
    spent: String,
    remaining: String,
    nonce: u64,
}

#[derive(Debug, Serialize)]
struct BalanceResponse {
    balance: String,
    has_channel: bool,
    channel_remaining: String,
}

#[derive(Debug, Serialize)]
struct InfoResponse {
    contract: String,
    service_address: String,
    chain_id: u64,
    rpc_url: String,
    enabled: bool,
}

#[derive(Debug, Serialize)]
struct PriceResponse {
    dot_usd: f64,
    sono_per_dot: String,
    sono_usd_base: f64,
    sono_usd_fiat: f64,
    fiat_premium_pct: f64,
    updated_ago_secs: u64,
}

#[derive(Debug, Serialize)]
struct SignStateResponse {
    spent: String,
    nonce: u64,
    signature: String,
}

#[derive(Debug, Deserialize)]
struct SettleRequest {
    user: String,
    signature: String,
}

/// GET /api/sono/info — contract and service info
async fn info(State(state): State<Arc<AppState>>) -> Json<InfoResponse> {
    match &state.sono {
        Some(sono) => Json(InfoResponse {
            contract: std::env::var("SONO_CONTRACT").unwrap_or_default(),
            service_address: format!("{}", sono.service_address()),
            chain_id: 420420417,
            rpc_url: std::env::var("SONO_RPC_URL").unwrap_or_default(),
            enabled: true,
        }),
        None => Json(InfoResponse {
            contract: String::new(),
            service_address: String::new(),
            chain_id: 0,
            rpc_url: String::new(),
            enabled: false,
        }),
    }
}

/// GET /api/sono/price — current SONO pricing info
async fn price(State(state): State<Arc<AppState>>) -> Json<PriceResponse> {
    match &state.sono {
        Some(sono) => {
            let p = sono.price.read().await;
            Json(PriceResponse {
                dot_usd: p.dot_usd,
                sono_per_dot: format!("{}", p.txt_per_dot),
                sono_usd_base: p.txt_usd_base,
                sono_usd_fiat: p.txt_usd_fiat,
                fiat_premium_pct: state.config.sono_fiat_premium * 100.0,
                updated_ago_secs: p.updated_at.elapsed().as_secs(),
            })
        }
        None => Json(PriceResponse {
            dot_usd: 0.0,
            sono_per_dot: "0".into(),
            sono_usd_base: 0.0,
            sono_usd_fiat: 0.0,
            fiat_premium_pct: 0.0,
            updated_ago_secs: 0,
        }),
    }
}

/// GET /api/sono/channel?user=0x... — get channel status
async fn channel_status(
    State(state): State<Arc<AppState>>,
    Query(q): Query<UserQuery>,
) -> Result<Json<ChannelResponse>, StatusCode> {
    let sono = state.sono.as_ref().ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let user: alloy::primitives::Address = q.user.parse().map_err(|_| StatusCode::BAD_REQUEST)?;

    match sono.get_channel(&user).await {
        Some(ch) => {
            let remaining = ch.deposit.saturating_sub(ch.spent);
            Ok(Json(ChannelResponse {
                has_channel: true,
                deposit: format!("{}", ch.deposit),
                spent: format!("{}", ch.spent),
                remaining: format!("{}", remaining),
                nonce: ch.nonce,
            }))
        }
        None => Ok(Json(ChannelResponse {
            has_channel: false,
            deposit: "0".into(),
            spent: "0".into(),
            remaining: "0".into(),
            nonce: 0,
        })),
    }
}

/// GET /api/sono/balance?user=0x... — quick balance check
async fn balance(
    State(state): State<Arc<AppState>>,
    Query(q): Query<UserQuery>,
) -> Result<Json<BalanceResponse>, StatusCode> {
    let sono = state.sono.as_ref().ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let user: alloy::primitives::Address = q.user.parse().map_err(|_| StatusCode::BAD_REQUEST)?;

    let remaining = sono.remaining(&user).await;
    let has_channel = remaining > alloy::primitives::U256::ZERO;

    Ok(Json(BalanceResponse {
        balance: format!("{}", remaining),
        has_channel,
        channel_remaining: format!("{}", remaining),
    }))
}

/// POST /api/sono/sign-state — get the service's signature over current state
async fn sign_state(
    State(state): State<Arc<AppState>>,
    Json(req): Json<UserQuery>,
) -> Result<Json<SignStateResponse>, StatusCode> {
    let sono = state.sono.as_ref().ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let user: alloy::primitives::Address = req.user.parse().map_err(|_| StatusCode::BAD_REQUEST)?;

    let (spent, nonce, sig) = sono.sign_state(&user).await.map_err(|e| {
        error!("sign_state failed: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    Ok(Json(SignStateResponse {
        spent: format!("{}", spent),
        nonce,
        signature: format!("0x{}", hex::encode(sig)),
    }))
}

/// POST /api/sono/settle — cooperatively close the channel
async fn settle(
    State(state): State<Arc<AppState>>,
    Json(req): Json<SettleRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let sono = state.sono.as_ref().ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let user: alloy::primitives::Address = req.user.parse().map_err(|_| StatusCode::BAD_REQUEST)?;
    let sig = hex::decode(req.signature.trim_start_matches("0x"))
        .map_err(|_| StatusCode::BAD_REQUEST)?;

    sono.settle(&user, sig).await.map_err(|e| {
        error!("settle failed: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    Ok(Json(serde_json::json!({ "status": "settled" })))
}
