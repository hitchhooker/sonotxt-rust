use sqlx::PgPool;
use std::sync::Arc;
use uuid::Uuid;

use crate::{ApiError, AppState, Result};
use hwpay::listener::penumbra::{PenumbraListener as HwPayListener, PenumbraListenerConfig};
use hwpay::listener::{Deposit, DepositCallback};
use hwpay::wallet::penumbra::PenumbraWallet;

/// parse pcli config.toml to extract spend_key
fn parse_pcli_config(path: &str) -> std::result::Result<String, String> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("failed to read pcli config at {}: {}", path, e))?;

    // parse toml
    let config: toml::Value = toml::from_str(&content)
        .map_err(|e| format!("failed to parse pcli config: {}", e))?;

    // extract custody.spend_key
    config.get("custody")
        .and_then(|c| c.get("spend_key"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| "no custody.spend_key found in pcli config".to_string())
}

/// default maximum addresses per user if not configured
const DEFAULT_MAX_ADDRESSES: i64 = 5;

/// callback handler that credits deposits to user accounts
struct PenumbraDepositHandler {
    db: PgPool,
}

#[async_trait::async_trait]
impl DepositCallback for PenumbraDepositHandler {
    async fn on_deposit(&self, deposit: Deposit) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // look up account by penumbra_index
        let account_id: Option<Uuid> = sqlx::query_scalar(
            r#"
            SELECT account_id FROM payment_addresses
            WHERE chain = 'penumbra' AND penumbra_index = $1
            "#,
        )
        .bind(deposit.derivation_index as i64)
        .fetch_optional(&self.db)
        .await?;

        let Some(account_id) = account_id else {
            tracing::warn!(
                "penumbra deposit for unknown penumbra_index {}: {}",
                deposit.derivation_index,
                deposit.tx_hash
            );
            return Ok(());
        };

        // check for duplicate deposit
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM deposits WHERE tx_hash = $1)"
        )
        .bind(&deposit.tx_hash)
        .fetch_one(&self.db)
        .await?;

        if exists {
            tracing::debug!("skipping duplicate penumbra deposit: {}", deposit.tx_hash);
            return Ok(());
        }

        // record the deposit
        let deposit_id = Uuid::new_v4();
        sqlx::query(
            r#"
            INSERT INTO deposits (id, account_id, chain, tx_hash, asset, amount, status, confirmations, to_address)
            VALUES ($1, $2, 'penumbra', $3, 'USDC', $4, 'confirmed', 1, $5)
            "#,
        )
        .bind(deposit_id)
        .bind(account_id)
        .bind(&deposit.tx_hash)
        .bind(deposit.amount)
        .bind(&deposit.to)
        .execute(&self.db)
        .await?;

        // credit the account balance
        super::credit_deposit(&self.db, deposit_id, account_id, deposit.amount, "penumbra", &deposit.tx_hash)
            .await
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) })?;

        tracing::info!(
            "credited penumbra deposit {} USDC to account {} (tx: {})",
            deposit.amount,
            account_id,
            deposit.tx_hash
        );

        Ok(())
    }
}

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
        // try to get wallet from spend key (bech32), pcli config, or hex seed
        let wallet = if let Some(spend_key) = &self.state.config.penumbra_spend_key {
            // direct bech32 spend key
            PenumbraWallet::from_spend_key_bech32(spend_key)
                .map_err(|e| ApiError::Internal(format!("invalid spend key: {}", e)))?
        } else if let Some(config_path) = &self.state.config.pcli_config_path {
            // read from pcli config.toml
            let spend_key = parse_pcli_config(config_path)
                .map_err(|e| ApiError::Internal(e))?;
            PenumbraWallet::from_spend_key_bech32(&spend_key)
                .map_err(|e| ApiError::Internal(format!("invalid spend key from pcli config: {}", e)))?
        } else if let Some(seed) = &self.state.config.deposit_wallet_seed {
            // hex seed fallback
            let seed_bytes = hex::decode(seed.trim_start_matches("0x"))
                .map_err(|e| ApiError::Internal(format!("invalid hex seed: {}", e)))?;
            PenumbraWallet::from_seed(&seed_bytes)
                .map_err(|e| ApiError::Internal(format!("invalid wallet seed: {}", e)))?
        } else {
            return Err(ApiError::Internal("no penumbra wallet configured (set PENUMBRA_SPEND_KEY, PCLI_CONFIG_PATH, or DEPOSIT_WALLET_SEED)".into()));
        };

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
    /// connects to pclientd view service to watch for incoming notes
    pub async fn run(&self) -> Result<()> {
        let rpc_url = match &self.state.config.penumbra_rpc {
            Some(url) => url.clone(),
            None => {
                tracing::info!("penumbra rpc not configured, skipping listener");
                return Ok(());
            }
        };

        tracing::info!("starting penumbra listener at {}", rpc_url);

        // create the hwpay listener with our callback
        let config = PenumbraListenerConfig {
            view_service_url: rpc_url,
        };
        let handler = PenumbraDepositHandler {
            db: self.state.db.clone(),
        };
        let listener = HwPayListener::new(config).with_callback(handler);

        // load all active penumbra addresses and register them with the listener
        let addresses: Vec<(i64, String, i32)> = sqlx::query_as(
            r#"
            SELECT penumbra_index, account_id::text, derivation_index
            FROM payment_addresses
            WHERE chain = 'penumbra' AND is_active = true AND penumbra_index IS NOT NULL
            "#,
        )
        .fetch_all(&self.state.db)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

        let addr_count = addresses.len();
        for (penumbra_index, account_id, derivation_index) in addresses {
            listener
                .watch(penumbra_index as u32, &account_id, derivation_index as u32)
                .await;
        }

        tracing::info!("registered {} penumbra addresses for monitoring", addr_count);

        // run the listener (blocks forever)
        listener
            .run()
            .await
            .map_err(|e| ApiError::Internal(e.to_string()))?;

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
