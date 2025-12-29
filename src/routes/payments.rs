use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use uuid::Uuid;

use crate::{
    auth::api_key::AuthenticatedUser,
    services::payments::{assethub::AssetHubListener, penumbra::PenumbraListener, stripe::StripeService},
    ApiError, AppState, Result,
};

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/payments/addresses", get(get_deposit_addresses))
        .route("/payments/addresses/polkadot/rotate", post(rotate_polkadot_address))
        .route("/payments/addresses/polkadot/list", get(list_polkadot_addresses))
        .route("/payments/stripe/checkout", post(create_stripe_checkout))
        .route("/payments/stripe/webhook", post(stripe_webhook))
        .route("/payments/deposits", get(list_deposits))
        .route("/payments/penumbra/deposit", post(record_penumbra_deposit))
}

#[derive(Serialize)]
pub struct DepositAddresses {
    polkadot_assethub: Option<String>,
    penumbra: Option<String>,
}

async fn get_deposit_addresses(
    State(state): State<Arc<AppState>>,
    user: AuthenticatedUser,
) -> Result<Json<DepositAddresses>> {
    let assethub_listener = AssetHubListener::new(state.clone());
    let penumbra_listener = PenumbraListener::new(state.clone());

    let polkadot_addr = assethub_listener
        .get_deposit_address(user.account_id)
        .await
        .ok();
    let penumbra_addr = penumbra_listener
        .get_deposit_address(user.account_id)
        .await
        .ok();

    Ok(Json(DepositAddresses {
        polkadot_assethub: polkadot_addr,
        penumbra: penumbra_addr,
    }))
}

#[derive(Deserialize)]
pub struct CreateCheckoutRequest {
    amount: f64,
}

#[derive(Serialize)]
pub struct CreateCheckoutResponse {
    url: String,
}

async fn create_stripe_checkout(
    State(state): State<Arc<AppState>>,
    user: AuthenticatedUser,
    Json(req): Json<CreateCheckoutRequest>,
) -> Result<Json<CreateCheckoutResponse>> {
    if req.amount < 5.0 || req.amount > 500.0 {
        return Err(ApiError::InvalidRequest(
            "amount must be between $5 and $500".into(),
        ));
    }

    let stripe = StripeService::new(&state.config);
    let url = stripe
        .create_checkout_session(user.account_id, req.amount)
        .await?;

    Ok(Json(CreateCheckoutResponse { url }))
}

async fn stripe_webhook(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<StatusCode> {
    let signature = headers
        .get("stripe-signature")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| ApiError::InvalidRequest("missing signature".into()))?;

    let payload =
        std::str::from_utf8(&body).map_err(|_| ApiError::InvalidRequest("invalid body".into()))?;

    StripeService::handle_webhook(&state.db, &state.config, payload, signature).await?;

    Ok(StatusCode::OK)
}

#[derive(Serialize)]
pub struct Deposit {
    id: String,
    chain: String,
    tx_hash: String,
    asset: String,
    amount: f64,
    status: String,
    created_at: String,
}

async fn list_deposits(
    State(state): State<Arc<AppState>>,
    user: AuthenticatedUser,
) -> Result<Json<Vec<Deposit>>> {
    let rows: Vec<(Uuid, String, String, String, f64, String, chrono::DateTime<chrono::Utc>)> =
        sqlx::query_as(
            r#"
        SELECT id, chain, tx_hash, asset, amount, status, created_at
        FROM deposits
        WHERE account_id = $1
        ORDER BY created_at DESC
        LIMIT 50
        "#,
        )
        .bind(user.account_id)
        .fetch_all(&state.db)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    let deposits = rows
        .into_iter()
        .map(|(id, chain, tx_hash, asset, amount, status, created_at)| Deposit {
            id: id.to_string(),
            chain,
            tx_hash,
            asset,
            amount,
            status,
            created_at: created_at.to_rfc3339(),
        })
        .collect();

    Ok(Json(deposits))
}

#[derive(Deserialize)]
pub struct RecordPenumbraDeposit {
    tx_hash: String,
    amount: f64,
}

#[derive(Serialize)]
pub struct RecordDepositResponse {
    deposit_id: String,
    status: String,
}

async fn record_penumbra_deposit(
    State(state): State<Arc<AppState>>,
    user: AuthenticatedUser,
    Json(req): Json<RecordPenumbraDeposit>,
) -> Result<Json<RecordDepositResponse>> {
    let deposit_id =
        PenumbraListener::record_deposit(&state.db, user.account_id, &req.tx_hash, req.amount)
            .await?;

    Ok(Json(RecordDepositResponse {
        deposit_id: deposit_id.to_string(),
        status: "pending".into(),
    }))
}

#[derive(Serialize)]
pub struct RotateAddressResponse {
    new_address: String,
    derivation_index: u32,
}

async fn rotate_polkadot_address(
    State(state): State<Arc<AppState>>,
    user: AuthenticatedUser,
) -> Result<Json<RotateAddressResponse>> {
    let listener = AssetHubListener::new(state.clone());
    let new_address = listener.rotate_address(user.account_id).await?;

    // get the derivation index
    let index: i32 = sqlx::query_scalar(
        r#"
        SELECT derivation_index
        FROM payment_addresses
        WHERE account_id = $1 AND chain = 'polkadot_assethub' AND is_active = true
        "#,
    )
    .bind(user.account_id)
    .fetch_one(&state.db)
    .await
    .map_err(|e| ApiError::Internal(e.to_string()))?;

    Ok(Json(RotateAddressResponse {
        new_address,
        derivation_index: index as u32,
    }))
}

#[derive(Serialize)]
pub struct PolkadotAddress {
    address: String,
    derivation_index: i32,
    is_active: bool,
}

async fn list_polkadot_addresses(
    State(state): State<Arc<AppState>>,
    user: AuthenticatedUser,
) -> Result<Json<Vec<PolkadotAddress>>> {
    let listener = AssetHubListener::new(state.clone());
    let addresses = listener.list_addresses(user.account_id).await?;

    let result = addresses
        .into_iter()
        .map(|(address, derivation_index, is_active)| PolkadotAddress {
            address,
            derivation_index,
            is_active,
        })
        .collect();

    Ok(Json(result))
}
