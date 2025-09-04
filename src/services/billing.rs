// src/services/billing.rs
use crate::{AppState, Result, error::ApiError};
use uuid::Uuid;
use chrono::{Utc, Duration};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct TtsRequest {
    pub minutes: f64,
    pub remove_watermark: bool,
    pub priority: bool,
    pub custom_voice: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct PricingEstimate {
    pub cost: f64,
    pub watermark: bool,
    pub subscriber_discount: f64,
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
    .await?
    .ok_or(ApiError::NotFound)?;
    
    let is_active_subscriber = account.subscription_type.is_some() && 
        account.subscription_expires.map_or(false, |exp| exp > Utc::now());
    
    let base_rate = if is_active_subscriber { 0.06 } else { 0.10 };
    let mut cost = request.minutes * base_rate;
    
    let watermark = if is_active_subscriber || account.watermark_free {
        false
    } else if request.remove_watermark {
        cost += request.minutes * 0.03;
        false
    } else {
        true
    };
    
    if request.priority && !is_active_subscriber {
        cost += request.minutes * 0.02;
    }
    
    if request.custom_voice.is_some() {
        let voice_rate = if is_active_subscriber { 0.02 } else { 0.05 };
        cost += request.minutes * voice_rate;
    }
    
    let subscriber_discount = if is_active_subscriber {
        request.minutes * (0.10 - 0.06) // Show savings
    } else {
        0.0
    };
    
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
    let estimate = estimate_cost(state, account_id, request).await?;
    
    // Check balance
    let balance = sqlx::query_scalar!(
        "SELECT balance FROM account_credits WHERE account_id = $1",
        account_id
    )
    .fetch_one(&state.db)
    .await?;
    
    if balance < estimate.cost {
        return Err(ApiError::InsufficientBalance);
    }
    
    // Start transaction
    let mut tx = state.db.begin().await?;
    
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
    .await?;
    
    // Log transaction
    sqlx::query!(
        r#"
        INSERT INTO transactions (account_id, amount, type, description)
        VALUES ($1, $2, 'usage', $3)
        "#,
        account_id,
        -estimate.cost,
        format!("TTS: {:.1} minutes for content {}", request.minutes, content_id)
    )
    .execute(&mut *tx)
    .await?;
    
    tx.commit().await?;
    
    Ok(estimate)
}

pub async fn add_credits(
    state: &AppState,
    account_id: Uuid,
    amount: f64,
    stripe_payment_id: Option<String>,
) -> Result<f64> {
    let mut tx = state.db.begin().await?;
    
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
    .await?;
    
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
    .await?;
    
    tx.commit().await?;
    
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
        _ => return Err(ApiError::InvalidRequest),
    };
    
    let expires = Utc::now() + Duration::days(30 * months);
    
    let mut tx = state.db.begin().await?;
    
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
    .await?;
    
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
    .await?;
    
    tx.commit().await?;
    
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
    .await?;
    
    Ok(())
}
