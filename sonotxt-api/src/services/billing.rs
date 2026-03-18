//! TXT-native billing service
//!
//! TXT is the only balance unit. 1 TXT = 10^10 raw units, priced at $0.01.
//! Users top up via Stripe (fiat) or on-chain purchase.
//! TTS usage deducts TXT from custodial balance or payment channel.

use sqlx::PgPool;
use uuid::Uuid;

use crate::error::{ApiError, Result};
use crate::services::sono::{PriceInfo, SonoService};

/// 1 TXT = 10^10 raw units (10 decimals)
pub const TXT_DECIMALS: u64 = 10_000_000_000;

/// Convert character count to TXT cost (raw units)
pub fn txt_cost_for_chars(char_count: usize, cost_per_char_usd: f64, price: &PriceInfo) -> i64 {
    let usd_cost = char_count as f64 * cost_per_char_usd;
    // usd_cost / txt_usd_base * 10^10
    (usd_cost / price.txt_usd_base * TXT_DECIMALS as f64) as i64
}

/// Format raw TXT units to human-readable string
pub fn format_txt(raw: i64) -> String {
    let whole = raw / TXT_DECIMALS as i64;
    let frac = (raw % TXT_DECIMALS as i64).abs();
    if frac == 0 {
        format!("{}", whole)
    } else {
        format!("{}.{:010}", whole, frac).trim_end_matches('0').to_string()
    }
}

/// Result of a charge operation
#[derive(Debug)]
pub struct ChargeResult {
    /// Amount charged from custodial DB balance
    pub from_custodial: i64,
    /// Amount charged from payment channel
    pub from_channel: i64,
}

/// Check balance and charge TXT for a TTS request.
/// Tries custodial balance first, then payment channel.
pub async fn check_and_charge(
    db: &PgPool,
    sono: Option<&SonoService>,
    user_id: Uuid,
    wallet_address: Option<&str>,
    txt_cost: i64,
) -> Result<ChargeResult> {
    if txt_cost <= 0 {
        return Ok(ChargeResult { from_custodial: 0, from_channel: 0 });
    }

    // Try custodial balance first
    let custodial_balance: i64 = sqlx::query_scalar(
        "SELECT txt_balance FROM users WHERE id = $1",
    )
    .bind(user_id)
    .fetch_optional(db)
    .await?
    .unwrap_or(0);

    if custodial_balance >= txt_cost {
        // Full charge from custodial
        let rows = sqlx::query(
            "UPDATE users SET txt_balance = txt_balance - $1 WHERE id = $2 AND txt_balance >= $1",
        )
        .bind(txt_cost)
        .bind(user_id)
        .execute(db)
        .await?
        .rows_affected();

        if rows > 0 {
            return Ok(ChargeResult { from_custodial: txt_cost, from_channel: 0 });
        }
    }

    // Try payment channel (if wallet connected and sono configured)
    if let (Some(sono), Some(wallet_addr)) = (sono, wallet_address) {
        let evm_addr = crate::routes::sono::ss58_to_h160(wallet_addr)
            .map_err(|_| ApiError::InsufficientBalance)?;

        let channel_remaining = sono.remaining(&evm_addr).await;
        let txt_cost_u256 = alloy::primitives::U256::from(txt_cost as u128);

        // If custodial covers part, deduct what we can from custodial, rest from channel
        if custodial_balance > 0 {
            let from_custodial = custodial_balance;
            let from_channel = txt_cost - custodial_balance;
            let from_channel_u256 = alloy::primitives::U256::from(from_channel as u128);

            if channel_remaining >= from_channel_u256 {
                // Deduct custodial part
                sqlx::query("UPDATE users SET txt_balance = 0 WHERE id = $1")
                    .bind(user_id)
                    .execute(db)
                    .await?;

                // Deduct channel part
                sono.charge(&evm_addr, from_channel_u256)
                    .await
                    .map_err(|_| ApiError::InsufficientBalance)?;

                return Ok(ChargeResult { from_custodial, from_channel });
            }
        }

        // Full charge from channel
        if channel_remaining >= txt_cost_u256 {
            sono.charge(&evm_addr, txt_cost_u256)
                .await
                .map_err(|_| ApiError::InsufficientBalance)?;

            return Ok(ChargeResult { from_custodial: 0, from_channel: txt_cost });
        }
    }

    Err(ApiError::InsufficientBalance)
}

/// Credit TXT to a user's custodial balance.
/// Used by Stripe webhook, on-chain deposits, admin grants.
pub async fn credit_txt(
    db: &PgPool,
    user_id: Uuid,
    txt_amount: i64,
    source: &str,
    ref_id: &str,
) -> Result<i64> {
    let mut tx = db.begin().await?;

    let new_balance: i64 = sqlx::query_scalar(
        "UPDATE users SET txt_balance = txt_balance + $1 WHERE id = $2 RETURNING txt_balance",
    )
    .bind(txt_amount)
    .bind(user_id)
    .fetch_one(&mut *tx)
    .await
    .map_err(|_| ApiError::InternalError)?;

    // Log transaction
    sqlx::query(
        "INSERT INTO transactions (account_id, amount, type, description) VALUES ($1, $2, 'credit', $3)",
    )
    .bind(user_id)
    .bind(txt_amount as f64 / TXT_DECIMALS as f64)
    .bind(format!("{} TXT from {} ({})", format_txt(txt_amount), source, ref_id))
    .execute(&mut *tx)
    .await
    .map_err(|_| ApiError::InternalError)?;

    tx.commit().await?;

    Ok(new_balance)
}

/// Convert fiat amount to TXT using the fiat rate (with premium)
pub fn fiat_to_txt(fiat_amount: f64, price: &PriceInfo) -> i64 {
    (fiat_amount / price.txt_usd_fiat * TXT_DECIMALS as f64) as i64
}

/// Get user's total available TXT (custodial + channel)
pub async fn total_balance(
    db: &PgPool,
    sono: Option<&SonoService>,
    user_id: Uuid,
    wallet_address: Option<&str>,
) -> Result<(i64, i64)> {
    let custodial: i64 = sqlx::query_scalar(
        "SELECT txt_balance FROM users WHERE id = $1",
    )
    .bind(user_id)
    .fetch_optional(db)
    .await?
    .unwrap_or(0);

    let channel = if let (Some(sono), Some(wallet_addr)) = (sono, wallet_address) {
        if let Ok(evm_addr) = crate::routes::sono::ss58_to_h160(wallet_addr) {
            let remaining = sono.remaining(&evm_addr).await;
            // Convert U256 to i64 (safe for reasonable balances)
            remaining.try_into().unwrap_or(i64::MAX)
        } else {
            0i64
        }
    } else {
        0i64
    };

    Ok((custodial, channel))
}
