/// ZID auth routes - zafu wallet identity
///
/// POST /auth/zid - authenticate with ZID signature, get session token
/// GET /auth/zid/me - get account info for authenticated ZID user
/// GET /auth/zid/deposit-address - get ZEC address for deposits

use axum::{
    extract::State,
    routing::get,
    Json, Router,
};
use serde::Serialize;
use std::sync::Arc;

use crate::{auth::zid::ZidUser, AppState, Result};

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/zid/me", get(me))
        .route("/zid/deposit-address", get(deposit_address))
}

#[derive(Serialize)]
pub struct ZidMeResponse {
    account_id: String,
    pubkey: String,
    balance: f64,
}

/// get account info - ZID auth via header
async fn me(
    State(state): State<Arc<AppState>>,
    zid: ZidUser,
) -> Result<Json<ZidMeResponse>> {
    let balance: f64 = sqlx::query_scalar(
        "SELECT COALESCE(balance, 0.0) FROM account_credits WHERE account_id = $1"
    )
    .bind(zid.account_id)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| crate::ApiError::Internal(e.to_string()))?
    .unwrap_or(0.0);

    Ok(Json(ZidMeResponse {
        account_id: zid.account_id.to_string(),
        pubkey: zid.pubkey,
        balance,
    }))
}

#[derive(Serialize)]
pub struct DepositAddressResponse {
    address: String,
    memo: String,
}

/// get a ZEC deposit address for this ZID user.
/// the memo field contains the ZID pubkey so the payment scanner
/// can credit the right account.
async fn deposit_address(
    State(state): State<Arc<AppState>>,
    zid: ZidUser,
) -> Result<Json<DepositAddressResponse>> {
    // use a single receiving address for simplicity.
    // the memo identifies the user.
    let address = state.config.zcash_deposit_address.clone()
        .unwrap_or_default();

    Ok(Json(DepositAddressResponse {
        address,
        memo: format!("tts:{}", zid.pubkey),
    }))
}
