//! asset hub deposit listener with proper subxt event decoding

use sqlx::PgPool;
use std::sync::Arc;
use uuid::Uuid;

use crate::{ApiError, AppState, Result};
use crate::services::wallet::WalletDeriver;

const _USDC_DECIMALS: u32 = 6;
const _USDT_DECIMALS: u32 = 6;

/// default maximum addresses per user if not configured
const DEFAULT_MAX_ADDRESSES: i64 = 5;

pub struct AssetHubService {
    state: Arc<AppState>,
    max_addresses_per_user: i64,
}

impl AssetHubService {
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

    /// manually record a deposit (detected off-chain or via RPC)
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

/// deposit listener using hwpay's subxt-based event decoder
pub struct AssetHubListener {
    state: Arc<AppState>,
    hwpay_listener: hwpay::AssetHubListener,
}

impl AssetHubListener {
    pub fn new(state: Arc<AppState>) -> Self {
        let rpc_url = state.config.assethub_rpc.clone();
        let hwpay_listener = hwpay::AssetHubListener::with_rpc_url(&rpc_url);
        Self { state, hwpay_listener }
    }

    /// load all active watched addresses from db
    async fn load_watched_addresses(&self) -> Result<()> {
        let rows: Vec<(String, Uuid, i32)> = sqlx::query_as(
            r#"
            SELECT address, account_id, derivation_index
            FROM payment_addresses
            WHERE chain = 'polkadot_assethub' AND is_active = true
            "#,
        )
        .fetch_all(&self.state.db)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

        let count = rows.len();
        for (address, account_id, derivation_index) in rows {
            self.hwpay_listener.watch(
                address,
                account_id.to_string(),
                derivation_index as u32,
            ).await;
        }

        tracing::info!("loaded {} watched addresses", count);
        Ok(())
    }

    /// run the deposit listener (background task)
    pub async fn run(&mut self) -> Result<()> {
        tracing::info!("starting assethub listener at {}", self.state.config.assethub_rpc);

        // connect to the network
        self.hwpay_listener.connect().await
            .map_err(|e| ApiError::Internal(format!("failed to connect: {}", e)))?;

        // load watched addresses
        self.load_watched_addresses().await?;

        // create callback that records deposits
        let callback = DepositHandler {
            db: self.state.db.clone(),
        };

        // run the listener
        self.hwpay_listener.run(callback).await
            .map_err(|e| ApiError::Internal(format!("listener error: {}", e)))?;

        Ok(())
    }

    /// add address to watch list
    pub async fn watch(&self, address: String, account_id: Uuid, index: u32) {
        self.hwpay_listener.watch(address, account_id.to_string(), index).await;
    }

    /// remove address from watch list
    pub async fn unwatch(&self, address: &str) {
        self.hwpay_listener.unwatch(address).await;
    }
}

/// callback handler for hwpay deposits
struct DepositHandler {
    db: PgPool,
}

#[async_trait::async_trait]
impl hwpay::DepositCallback for DepositHandler {
    async fn on_deposit(&self, deposit: hwpay::Deposit) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let account_id = Uuid::parse_str(&deposit.user_id)?;

        // check for duplicate
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM deposits WHERE tx_hash = $1)"
        )
        .bind(&deposit.tx_hash)
        .fetch_one(&self.db)
        .await?;

        if exists {
            tracing::debug!("deposit already recorded: {}", deposit.tx_hash);
            return Ok(());
        }

        // record the deposit
        let deposit_id = Uuid::new_v4();
        let asset = format!("{}", deposit.asset);

        sqlx::query(
            r#"
            INSERT INTO deposits (id, account_id, chain, tx_hash, asset, amount, block_number, from_address, to_address, status, confirmations)
            VALUES ($1, $2, 'polkadot_assethub', $3, $4, $5, $6, $7, $8, 'confirmed', 1)
            "#,
        )
        .bind(deposit_id)
        .bind(account_id)
        .bind(&deposit.tx_hash)
        .bind(&asset)
        .bind(deposit.amount)
        .bind(deposit.block_number as i64)
        .bind(&deposit.from)
        .bind(&deposit.to)
        .execute(&self.db)
        .await?;

        tracing::info!(
            "recorded deposit {} for account {}: {} {}",
            deposit_id,
            account_id,
            deposit.amount,
            asset
        );

        // credit immediately since we're already on finalized blocks
        super::credit_deposit(
            &self.db,
            deposit_id,
            account_id,
            deposit.amount,
            "polkadot_assethub",
            &deposit.tx_hash,
        ).await.map_err(|e| Box::new(std::io::Error::new(std::io::ErrorKind::Other, e.to_string())))?;

        Ok(())
    }
}

// keep the old struct name for compatibility but point to new one
pub type AssetHubListener_Legacy = AssetHubService;
