use serde::Deserialize;
use sqlx::PgPool;
use std::sync::Arc;
use uuid::Uuid;

use crate::{ApiError, AppState, Result};
use crate::services::wallet::WalletDeriver;

const _USDC_DECIMALS: u32 = 6;
const _USDT_DECIMALS: u32 = 6;

/// default maximum addresses per user if not configured
const DEFAULT_MAX_ADDRESSES: i64 = 5;

pub struct AssetHubListener {
    state: Arc<AppState>,
    max_addresses_per_user: i64,
}

#[derive(Deserialize, Debug)]
struct RpcResponse<T> {
    result: Option<T>,
    #[allow(dead_code)]
    error: Option<serde_json::Value>,
}

#[derive(Deserialize, Debug)]
struct BlockHeader {
    number: String,
}

#[derive(Deserialize, Debug)]
struct SignedBlock {
    block: Block,
}

#[derive(Deserialize, Debug)]
struct Block {
    header: BlockHeader,
    extrinsics: Vec<String>,
}

impl AssetHubListener {
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

    pub fn set_max_addresses(&mut self, max: i64) {
        self.max_addresses_per_user = max;
    }

    /// generate or retrieve the active deposit address for an account
    pub async fn get_deposit_address(&self, account_id: Uuid) -> Result<String> {
        // check for existing active address
        let existing: Option<(String, i32)> = sqlx::query_as(
            r#"
            SELECT address, derivation_index
            FROM payment_addresses
            WHERE account_id = $1 AND chain = 'polkadot_assethub' AND is_active = true
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

    /// rotate wallet - deactivate current and derive new address with next index
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
            WHERE account_id = $1 AND chain = 'polkadot_assethub'
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
            WHERE account_id = $1 AND chain = 'polkadot_assethub'
            "#,
        )
        .bind(account_id)
        .execute(&self.state.db)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

        // derive new address
        self.derive_and_store_address(account_id, new_index as u32).await
    }

    /// count total addresses for an account
    async fn count_addresses(&self, account_id: Uuid) -> Result<i64> {
        let count: i64 = sqlx::query_scalar(
            r#"
            SELECT COUNT(*)
            FROM payment_addresses
            WHERE account_id = $1 AND chain = 'polkadot_assethub'
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

        // create wallet deriver from seed
        let deriver = WalletDeriver::from_seed_hex(seed)
            .or_else(|_| WalletDeriver::from_mnemonic(seed))
            .map_err(|e| ApiError::Internal(format!("invalid wallet seed: {}", e)))?;

        // derive address using account_id as user identifier
        let address = deriver.derive_polkadot_address(&account_id.to_string(), derivation_index);

        // store address
        sqlx::query(
            r#"
            INSERT INTO payment_addresses (account_id, chain, address, derivation_index, is_active)
            VALUES ($1, 'polkadot_assethub', $2, $3, true)
            "#,
        )
        .bind(account_id)
        .bind(&address)
        .bind(derivation_index as i32)
        .execute(&self.state.db)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

        tracing::info!(
            "derived polkadot address for account {} index {}: {}",
            account_id,
            derivation_index,
            address
        );

        Ok(address)
    }

    /// check how many address slots remain for an account
    pub async fn remaining_address_slots(&self, account_id: Uuid) -> Result<i64> {
        let count = self.count_addresses(account_id).await?;
        Ok((self.max_addresses_per_user - count).max(0))
    }

    /// list all addresses for an account (including rotated ones)
    pub async fn list_addresses(&self, account_id: Uuid) -> Result<Vec<(String, i32, bool)>> {
        let rows: Vec<(String, i32, bool)> = sqlx::query_as(
            r#"
            SELECT address, derivation_index, is_active
            FROM payment_addresses
            WHERE account_id = $1 AND chain = 'polkadot_assethub'
            ORDER BY derivation_index DESC
            "#,
        )
        .bind(account_id)
        .fetch_all(&self.state.db)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

        Ok(rows)
    }

    /// run the deposit listener (background task)
    pub async fn run(&self) -> Result<()> {
        tracing::info!("starting assethub listener at {}", self.state.config.assethub_rpc);

        let mut last_block: u64 = 0;

        loop {
            match self.poll_blocks(&mut last_block).await {
                Ok(_) => {}
                Err(e) => {
                    tracing::error!("assethub poll error: {}", e);
                }
            }
            tokio::time::sleep(tokio::time::Duration::from_secs(6)).await;
        }
    }

    async fn poll_blocks(&self, last_block: &mut u64) -> Result<()> {
        let rpc_url = self.state.config.assethub_rpc.replace("wss://", "https://").replace("ws://", "http://");

        // get latest finalized block
        let resp: RpcResponse<String> = self
            .state
            .http
            .post(&rpc_url)
            .json(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "chain_getFinalizedHead",
                "params": []
            }))
            .send()
            .await
            .map_err(|e| ApiError::Internal(e.to_string()))?
            .json()
            .await
            .map_err(|e| ApiError::Internal(e.to_string()))?;

        let head_hash = resp.result.ok_or_else(|| ApiError::Internal("no finalized head".into()))?;

        // get block
        let block_resp: RpcResponse<SignedBlock> = self
            .state
            .http
            .post(&rpc_url)
            .json(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "chain_getBlock",
                "params": [head_hash]
            }))
            .send()
            .await
            .map_err(|e| ApiError::Internal(e.to_string()))?
            .json()
            .await
            .map_err(|e| ApiError::Internal(e.to_string()))?;

        let block = block_resp.result.ok_or_else(|| ApiError::Internal("no block".into()))?;
        let block_num = u64::from_str_radix(&block.block.header.number[2..], 16)
            .map_err(|e| ApiError::Internal(e.to_string()))?;

        if block_num <= *last_block {
            return Ok(());
        }

        tracing::debug!("processing block {}", block_num);
        *last_block = block_num;

        // process extrinsics
        for (idx, ext) in block.block.extrinsics.iter().enumerate() {
            self.process_extrinsic(ext, block_num, idx).await;
        }

        Ok(())
    }

    async fn process_extrinsic(&self, ext_hex: &str, _block_num: u64, _idx: usize) {
        let _ext_bytes = match hex::decode(ext_hex.trim_start_matches("0x")) {
            Ok(b) => b,
            Err(_) => return,
        };

        // check for assets pallet transfers
        // real implementation needs proper SCALE decoding
        self.check_pending_deposits().await;
    }

    async fn check_pending_deposits(&self) {
        let pending: Vec<(Uuid, Uuid, f64, String)> = match sqlx::query_as(
            r#"
            SELECT id, account_id, amount, tx_hash
            FROM deposits
            WHERE chain = 'polkadot_assethub' AND status = 'pending'
            "#,
        )
        .fetch_all(&self.state.db)
        .await
        {
            Ok(p) => p,
            Err(_) => return,
        };

        for (deposit_id, account_id, amount, tx_hash) in pending {
            let created: chrono::DateTime<chrono::Utc> = match sqlx::query_scalar(
                "SELECT created_at FROM deposits WHERE id = $1",
            )
            .bind(deposit_id)
            .fetch_one(&self.state.db)
            .await
            {
                Ok(c) => c,
                Err(_) => continue,
            };

            // auto-confirm after 2 minutes (placeholder)
            if chrono::Utc::now() - created > chrono::Duration::minutes(2) {
                let _ = sqlx::query(
                    "UPDATE deposits SET status = 'confirmed', confirmations = 1 WHERE id = $1",
                )
                .bind(deposit_id)
                .execute(&self.state.db)
                .await;

                if let Err(e) = super::credit_deposit(
                    &self.state.db,
                    deposit_id,
                    account_id,
                    amount,
                    "polkadot_assethub",
                    &tx_hash,
                )
                .await
                {
                    tracing::error!("failed to credit deposit: {}", e);
                }
            }
        }
    }

    /// manually record a deposit
    pub async fn record_deposit(
        db: &PgPool,
        account_id: Uuid,
        tx_hash: &str,
        asset: &str,
        amount: f64,
    ) -> Result<Uuid> {
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
            VALUES ($1, $2, 'polkadot_assethub', $3, $4, $5, 'pending')
            "#,
        )
        .bind(deposit_id)
        .bind(account_id)
        .bind(tx_hash)
        .bind(asset)
        .bind(amount)
        .execute(db)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

        Ok(deposit_id)
    }
}
