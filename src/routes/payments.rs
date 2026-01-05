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
    services::payments::{assethub::AssetHubService, penumbra::PenumbraListener, credit_deposit},
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
    let assethub_service = AssetHubService::new(state.clone());
    let penumbra_listener = PenumbraListener::new(state.clone());

    let polkadot_addr = assethub_service
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
    /// Currency: "eur" (default) or "usd"
    #[serde(default)]
    currency: Option<String>,
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
            "amount must be between €5 and €500".into(),
        ));
    }

    let currency = match req.currency.as_deref() {
        Some("usd") | Some("USD") => "usd",
        _ => "eur", // default to EUR for EU company
    };

    // Use hwpay's TPM-sealed Stripe processor
    let mut payments = state.payments.write().await;
    let stripe = payments.stripe()
        .map_err(|e| ApiError::Internal(format!("stripe not configured: {}", e)))?;

    let amount_cents = (req.amount * 100.0) as u64;
    let success_url = format!("{}/billing/success?session_id={{CHECKOUT_SESSION_ID}}", state.config.app_url);
    let cancel_url = format!("{}/billing", state.config.app_url);

    let metadata = serde_json::json!({
        "account_id": user.account_id.to_string(),
    });

    let session = stripe.create_checkout_session(
        amount_cents,
        currency,
        &success_url,
        &cancel_url,
        Some(&metadata),
    ).await.map_err(|e| ApiError::Internal(e.to_string()))?;

    let url = session.url
        .ok_or_else(|| ApiError::Internal("no checkout url".into()))?;

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

    // Verify signature using hwpay's TPM-sealed webhook secret
    let mut payments = state.payments.write().await;
    let stripe = payments.stripe()
        .map_err(|e| ApiError::Internal(format!("stripe not configured: {}", e)))?;

    stripe.verify_webhook(&body, signature)
        .map_err(|e| ApiError::InvalidRequest(format!("invalid signature: {}", e)))?;

    // Parse the event
    let event: hwpay::WebhookEvent = serde_json::from_slice(&body)
        .map_err(|e| ApiError::InvalidRequest(format!("invalid event: {}", e)))?;

    if event.event_type == "checkout.session.completed" {
        handle_checkout_complete(&state, &event.data.object).await?;
    }

    Ok(StatusCode::OK)
}

async fn handle_checkout_complete(state: &AppState, session: &serde_json::Value) -> Result<()> {
    let account_id: Uuid = session
        .get("metadata")
        .and_then(|m| m.get("account_id"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| ApiError::Internal("no account_id in metadata".into()))?
        .parse()
        .map_err(|_| ApiError::Internal("invalid account id".into()))?;

    let amount_cents = session
        .get("amount_total")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let amount = amount_cents as f64 / 100.0;

    let currency = session
        .get("currency")
        .and_then(|v| v.as_str())
        .unwrap_or("eur")
        .to_uppercase();

    let payment_id = session
        .get("payment_intent")
        .or_else(|| session.get("id"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();

    // Check for duplicate
    let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM deposits WHERE tx_hash = $1)")
        .bind(&payment_id)
        .fetch_one(&state.db)
        .await
        .unwrap_or(false);

    if exists {
        return Ok(());
    }

    // Record deposit
    let deposit_id = Uuid::new_v4();
    sqlx::query(
        r#"
        INSERT INTO deposits (id, account_id, chain, tx_hash, asset, amount, status, confirmations)
        VALUES ($1, $2, 'stripe', $3, $4, $5, 'confirmed', 1)
        "#,
    )
    .bind(deposit_id)
    .bind(account_id)
    .bind(&payment_id)
    .bind(&currency)
    .bind(amount)
    .execute(&state.db)
    .await
    .map_err(|e| ApiError::Internal(e.to_string()))?;

    // Credit account
    credit_deposit(&state.db, deposit_id, account_id, amount, "stripe", &payment_id).await?;

    tracing::info!("stripe payment {} credited to account {}: {} {}", payment_id, account_id, amount, currency);

    Ok(())
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
    let service = AssetHubService::new(state.clone());
    let new_address = service.rotate_address(user.account_id).await?;

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
    let service = AssetHubService::new(state.clone());
    let addresses = service.list_addresses(user.account_id).await?;

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
