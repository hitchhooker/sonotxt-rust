// src/services/billing.rs
use crate::{AppState, error::Result, error::ApiError};
use uuid::Uuid;
use chrono::{Utc, Duration};
use serde::{Deserialize, Serialize};

// deepinfra kokoro base: $0.80/M chars = $0.0000008/char
pub const DEEPINFRA_BASE: f64 = 0.0000008;
// markup multipliers over deepinfra cost
pub const US_MARKUP: f64 = 2.0;       // $1.60/M chars
pub const UK_MARKUP: f64 = 2.7;       // $2.16/M (2x * 1.35 gbp/usd)
// subscriber discount (40% off)
pub const SUBSCRIBER_DISCOUNT: f64 = 0.4;

#[derive(Debug, Serialize, Deserialize)]
pub struct TtsRequest {
    pub chars: usize,
    pub voice: String,
    pub remove_watermark: bool,
    pub priority: bool,
}

#[derive(Debug, Serialize)]
pub struct PricingEstimate {
    pub cost: f64,
    pub watermark: bool,
    pub subscriber_discount: f64,
}

// british voices (bf_* and bm_*) get higher markup
fn is_british_voice(voice: &str) -> bool {
    voice.starts_with("bf_") || voice.starts_with("bm_")
}

pub async fn estimate_cost(
    state: &AppState,
    account_id: Uuid,
    request: &TtsRequest,
) -> Result<PricingEstimate> {
    let account = sqlx::query!(
        r#"
        SELECT balance, subscription_type, watermark_free, subscription_expires
        FROM account_credits
        WHERE account_id = $1
        "#,
        account_id
    )
    .fetch_optional(&state.db)
    .await.map_err(|_| ApiError::InternalError)?
    .ok_or(ApiError::NotFound)?;

    let is_active_subscriber = account.subscription_type.is_some() &&
        account.subscription_expires.map_or(false, |exp| exp > Utc::now());

    // cost = deepinfra_base * markup
    let markup = if is_british_voice(&request.voice) {
        UK_MARKUP   // 2.7x = $2.16/M (2x * gbp/usd)
    } else {
        US_MARKUP   // 2.0x = $1.60/M
    };
    let mut cost = request.chars as f64 * DEEPINFRA_BASE * markup;

    // subscriber discount (40% off)
    let subscriber_discount = if is_active_subscriber {
        let discount = cost * SUBSCRIBER_DISCOUNT;
        cost -= discount;
        discount
    } else {
        0.0
    };

    let watermark = if is_active_subscriber || account.watermark_free {
        false
    } else if request.remove_watermark {
        // watermark removal: +25%
        cost *= 1.25;
        false
    } else {
        true
    };

    if request.priority && !is_active_subscriber {
        // priority: +20%
        cost *= 1.20;
    }

    Ok(PricingEstimate {
        cost,
        watermark,
        subscriber_discount,
    })
}

pub async fn charge_for_tts(
    state: &AppState,
    account_id: Uuid,
    request: &TtsRequest,
    content_id: Uuid,
) -> Result<PricingEstimate> {
    let estimate = estimate_cost(state, account_id, request).await.map_err(|_| ApiError::InternalError)?;

    // Check balance
    let balance = sqlx::query_scalar!(
        "SELECT balance FROM account_credits WHERE account_id = $1",
        account_id
    )
    .fetch_one(&state.db)
    .await.map_err(|_| ApiError::InternalError)?;

    if balance < estimate.cost {
        return Err(ApiError::InsufficientBalance);
    }

    // Start transaction
    let mut tx = state.db.begin().await.map_err(|_| ApiError::InternalError)?;

    // Deduct credits
    sqlx::query!(
        r#"
        UPDATE account_credits 
        SET balance = balance - $1, updated_at = NOW()
        WHERE account_id = $2
        "#,
        estimate.cost,
        account_id
    )
    .execute(&mut *tx)
    .await.map_err(|_| ApiError::InternalError)?;

    // Log transaction
    sqlx::query!(
        r#"
        INSERT INTO transactions (account_id, amount, type, description)
        VALUES ($1, $2, 'usage', $3)
        "#,
        account_id,
        -estimate.cost,
        format!("TTS: {} chars ({}) for {}", request.chars, request.voice, content_id)
    )
    .execute(&mut *tx)
    .await.map_err(|_| ApiError::InternalError)?;

    tx.commit().await.map_err(|_| ApiError::InternalError)?;

    Ok(estimate)
}

pub async fn add_credits(
    state: &AppState,
    account_id: Uuid,
    amount: f64,
    stripe_payment_id: Option<String>,
) -> Result<f64> {
    let mut tx = state.db.begin().await.map_err(|_| ApiError::InternalError)?;
    
    // Update balance
    let new_balance = sqlx::query_scalar!(
        r#"
        UPDATE account_credits 
        SET balance = balance + $1, updated_at = NOW()
        WHERE account_id = $2
        RETURNING balance
        "#,
        amount,
        account_id
    )
    .fetch_one(&mut *tx)
    .await.map_err(|_| ApiError::InternalError)?;
    
    // Log transaction
    sqlx::query!(
        r#"
        INSERT INTO transactions (account_id, amount, type, description, stripe_payment_id)
        VALUES ($1, $2, 'purchase', $3, $4)
        "#,
        account_id,
        amount,
        format!("Added ${:.2} credits", amount),
        stripe_payment_id
    )
    .execute(&mut *tx)
    .await.map_err(|_| ApiError::InternalError)?;
    
    tx.commit().await.map_err(|_| ApiError::InternalError)?;
    
    Ok(new_balance)
}

pub async fn activate_subscription(
    state: &AppState,
    account_id: Uuid,
    plan: &str,
    stripe_subscription_id: String,
) -> Result<()> {
    let (credits, months) = match plan {
        "monthly" => (15.0, 1),
        "yearly" => (180.0, 12), // 12 * 15
        _ => return Err(ApiError::InvalidRequestError),
    };
    
    let expires = Utc::now() + Duration::days(30 * months);
    
    let mut tx = state.db.begin().await.map_err(|_| ApiError::InternalError)?;
    
    // Update subscription
    sqlx::query!(
        r#"
        UPDATE account_credits 
        SET 
            subscription_type = $1,
            subscription_expires = $2,
            watermark_free = true,
            stripe_subscription_id = $3,
            balance = balance + $4,
            updated_at = NOW()
        WHERE account_id = $5
        "#,
        plan,
        expires,
        stripe_subscription_id,
        credits,
        account_id
    )
    .execute(&mut *tx)
    .await.map_err(|_| ApiError::InternalError)?;
    
    // Log transaction
    sqlx::query!(
        r#"
        INSERT INTO transactions (account_id, amount, type, description)
        VALUES ($1, $2, 'subscription', $3)
        "#,
        account_id,
        credits,
        format!("{} subscription: ${:.2} credits + no watermark", plan, credits)
    )
    .execute(&mut *tx)
    .await.map_err(|_| ApiError::InternalError)?;
    
    tx.commit().await.map_err(|_| ApiError::InternalError)?;
    
    Ok(())
}

pub async fn check_subscription_expiry(state: &AppState) -> Result<()> {
    // Run this as a daily cron job
    sqlx::query!(
        r#"
        UPDATE account_credits 
        SET 
            subscription_type = NULL,
            watermark_free = false
        WHERE subscription_expires < NOW()
        "#
    )
    .execute(&state.db)
    .await.map_err(|_| ApiError::InternalError)?;
    
    Ok(())
}
