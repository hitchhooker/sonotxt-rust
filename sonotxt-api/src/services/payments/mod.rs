pub mod assethub;
pub mod penumbra;
pub mod stripe;

use sqlx::PgPool;
use uuid::Uuid;

use crate::{ApiError, Result};

/// credit account after confirmed deposit
pub async fn credit_deposit(
    db: &PgPool,
    deposit_id: Uuid,
    account_id: Uuid,
    amount: f64,
    chain: &str,
    tx_hash: &str,
) -> Result<()> {
    let mut tx = db.begin().await.map_err(|e| ApiError::Internal(e.to_string()))?;

    // update deposit status
    sqlx::query(
        r#"
        UPDATE deposits
        SET status = 'credited', credited_at = NOW()
        WHERE id = $1
        "#,
    )
    .bind(deposit_id)
    .execute(&mut *tx)
    .await
    .map_err(|e| ApiError::Internal(e.to_string()))?;

    // add balance
    sqlx::query(
        r#"
        UPDATE account_credits
        SET balance = balance + $1, updated_at = NOW()
        WHERE account_id = $2
        "#,
    )
    .bind(amount)
    .bind(account_id)
    .execute(&mut *tx)
    .await
    .map_err(|e| ApiError::Internal(e.to_string()))?;

    // log transaction
    sqlx::query(
        r#"
        INSERT INTO transactions (account_id, amount, type, description, chain, tx_hash, deposit_id)
        VALUES ($1, $2, 'purchase', $3, $4, $5, $6)
        "#,
    )
    .bind(account_id)
    .bind(amount)
    .bind(format!("deposit {} via {}", amount, chain))
    .bind(chain)
    .bind(tx_hash)
    .bind(deposit_id)
    .execute(&mut *tx)
    .await
    .map_err(|e| ApiError::Internal(e.to_string()))?;

    tx.commit().await.map_err(|e| ApiError::Internal(e.to_string()))?;

    tracing::info!(
        "credited {} to account {} from {} tx {}",
        amount,
        account_id,
        chain,
        tx_hash
    );

    Ok(())
}
