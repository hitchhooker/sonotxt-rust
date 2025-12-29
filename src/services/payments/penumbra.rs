use sqlx::PgPool;
use std::sync::Arc;
use uuid::Uuid;

use crate::{ApiError, AppState, Result};
use hwpay::wallet::penumbra::PenumbraWallet;

/// default maximum addresses per user if not configured
const DEFAULT_MAX_ADDRESSES: i64 = 5;

pub struct PenumbraListener {
    state: Arc<AppState>,
    max_addresses_per_user: i64,
}

impl PenumbraListener {
    pub fn new(state: Arc<AppState>) -> Self {
        Self {
            state,
            max_addresses_per_user: DEFAULT_MAX_ADDRESSES,
        }
    }

    pub fn with_max_addresses(state: Arc<AppState>, max_addresses: i64) -> Self {
        Self {
            state,
            max_addresses_per_user: max_addresses,
        }
    }

    /// get or generate a penumbra deposit address for an account
    pub async fn get_deposit_address(&self, account_id: Uuid) -> Result<String> {
        // check for existing active address
        let existing: Option<(String, i32)> = sqlx::query_as(
            r#"
            SELECT address, derivation_index
            FROM payment_addresses
            WHERE account_id = $1 AND chain = 'penumbra' AND is_active = true
            "#,
        )
        .bind(account_id)
        .fetch_optional(&self.state.db)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

        if let Some((addr, _)) = existing {
            return Ok(addr);
        }

        // derive new address
        self.derive_and_store_address(account_id, 0).await
    }

    /// rotate wallet - deactivate current and derive new address
    pub async fn rotate_address(&self, account_id: Uuid) -> Result<String> {
        // check address limit
        let count = self.count_addresses(account_id).await?;
        if count >= self.max_addresses_per_user {
            return Err(ApiError::InvalidRequest(format!(
                "address limit reached ({}/{})",
                count, self.max_addresses_per_user
            )));
        }

        // get current max derivation index
        let max_index: Option<i32> = sqlx::query_scalar(
            r#"
            SELECT MAX(derivation_index)
            FROM payment_addresses
            WHERE account_id = $1 AND chain = 'penumbra'
            "#,
        )
        .bind(account_id)
        .fetch_optional(&self.state.db)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .flatten();

        let new_index = max_index.unwrap_or(-1) + 1;

        // deactivate all current addresses
        sqlx::query(
            r#"
            UPDATE payment_addresses
            SET is_active = false
            WHERE account_id = $1 AND chain = 'penumbra'
            "#,
        )
        .bind(account_id)
        .execute(&self.state.db)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

        self.derive_and_store_address(account_id, new_index as u32).await
    }

    async fn count_addresses(&self, account_id: Uuid) -> Result<i64> {
        let count: i64 = sqlx::query_scalar(
            r#"
            SELECT COUNT(*)
            FROM payment_addresses
            WHERE account_id = $1 AND chain = 'penumbra'
            "#,
        )
        .bind(account_id)
        .fetch_one(&self.state.db)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

        Ok(count)
    }

    async fn derive_and_store_address(&self, account_id: Uuid, derivation_index: u32) -> Result<String> {
        let seed = self
            .state
            .config
            .deposit_wallet_seed
            .as_ref()
            .ok_or_else(|| ApiError::Internal("no deposit wallet seed configured".into()))?;

        // create penumbra wallet from seed
        let seed_bytes = hex::decode(seed.trim_start_matches("0x"))
            .map_err(|e| ApiError::Internal(format!("invalid hex seed: {}", e)))?;

        let wallet = PenumbraWallet::from_seed(&seed_bytes)
            .map_err(|e| ApiError::Internal(format!("invalid wallet seed: {}", e)))?;

        // derive address using account_id as user identifier
        // returns (address, penumbra_index) - penumbra_index is used to map deposits back to user
        let (address, penumbra_index) = wallet.derive_address(&account_id.to_string(), derivation_index);

        // store address with penumbra_index for deposit mapping
        sqlx::query(
            r#"
            INSERT INTO payment_addresses (account_id, chain, address, derivation_index, penumbra_index, is_active)
            VALUES ($1, 'penumbra', $2, $3, $4, true)
            "#,
        )
        .bind(account_id)
        .bind(&address)
        .bind(derivation_index as i32)
        .bind(penumbra_index as i64)
        .execute(&self.state.db)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

        tracing::info!(
            "derived penumbra address for account {} derivation={} penumbra_index={}: {}",
            account_id,
            derivation_index,
            penumbra_index,
            address
        );

        Ok(address)
    }

    /// run the penumbra deposit listener
    /// connects to pclientd or view service to watch for incoming notes
    pub async fn run(&self) -> Result<()> {
        let rpc_url = match &self.state.config.penumbra_rpc {
            Some(url) => url.clone(),
            None => {
                tracing::info!("penumbra rpc not configured, skipping listener");
                return Ok(());
            }
        };

        tracing::info!("starting penumbra listener at {}", rpc_url);

        // connect to penumbra view service via grpc
        // this would use tonic to connect to the view service
        // and watch for new notes that match our payment addresses

        loop {
            if let Err(e) = self.poll_deposits().await {
                tracing::error!("penumbra poll error: {}", e);
            }
            tokio::time::sleep(tokio::time::Duration::from_secs(30)).await;
        }
    }

    async fn poll_deposits(&self) -> Result<()> {
        // in production, this would:
        // 1. connect to view service
        // 2. call NotesForVoting or similar to get unspent notes
        // 3. filter for notes matching our addresses
        // 4. check if we've already credited them
        // 5. credit new deposits

        // for now, this is a placeholder
        // the actual implementation requires penumbra-view crate
        // which needs the full penumbra dependency tree

        // check for pending deposits to confirm
        let pending: Vec<(Uuid, Uuid, f64, String)> = sqlx::query_as(
            r#"
            SELECT id, account_id, amount, tx_hash
            FROM deposits
            WHERE chain = 'penumbra' AND status = 'pending'
            "#,
        )
        .fetch_all(&self.state.db)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

        for (deposit_id, account_id, amount, tx_hash) in pending {
            // verify transaction on chain
            // for now, auto-confirm after some time (placeholder)
            let created: chrono::DateTime<chrono::Utc> = sqlx::query_scalar(
                "SELECT created_at FROM deposits WHERE id = $1",
            )
            .bind(deposit_id)
            .fetch_one(&self.state.db)
            .await
            .map_err(|e| ApiError::Internal(e.to_string()))?;

            // auto-confirm after 5 minutes (placeholder - real impl checks chain)
            if chrono::Utc::now() - created > chrono::Duration::minutes(5) {
                sqlx::query("UPDATE deposits SET status = 'confirmed', confirmations = 1 WHERE id = $1")
                    .bind(deposit_id)
                    .execute(&self.state.db)
                    .await
                    .map_err(|e| ApiError::Internal(e.to_string()))?;

                super::credit_deposit(&self.state.db, deposit_id, account_id, amount, "penumbra", &tx_hash)
                    .await?;
            }
        }

        Ok(())
    }

    /// manually record a penumbra deposit (for testing or manual verification)
    pub async fn record_deposit(
        db: &PgPool,
        account_id: Uuid,
        tx_hash: &str,
        amount: f64,
    ) -> Result<Uuid> {
        // check for duplicate
        let exists: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM deposits WHERE tx_hash = $1)")
                .bind(tx_hash)
                .fetch_one(db)
                .await
                .unwrap_or(false);

        if exists {
            return Err(ApiError::InvalidRequest("deposit already recorded".into()));
        }

        let deposit_id = Uuid::new_v4();
        sqlx::query(
            r#"
            INSERT INTO deposits (id, account_id, chain, tx_hash, asset, amount, status)
            VALUES ($1, $2, 'penumbra', $3, 'USDC', $4, 'pending')
            "#,
        )
        .bind(deposit_id)
        .bind(account_id)
        .bind(tx_hash)
        .bind(amount)
        .execute(db)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

        Ok(deposit_id)
    }
}
