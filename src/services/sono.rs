//! SONO payment channel service + price oracle
//!
//! Interacts with the SonoToken contract on Asset Hub via eth-RPC.
//! The API server acts as the service provider side of payment channels:
//!
//! 1. Watches for ChannelOpened events → grants user off-chain credits
//! 2. Tracks cumulative spend per channel, signs state updates
//! 3. Settles channels via cooperativeClose when user requests it
//!
//! Price oracle:
//! - Queries Asset Hub AssetConversion pallet for DOT/USDC pool reserves
//! - Computes DOT price in USD → sonoPerDot
//! - Calls setPrice() on the SonoToken contract
//! - Exposes rates for Stripe/fiat billing (with configurable premium)

use alloy::{
    network::EthereumWallet,
    primitives::{Address, U256},
    providers::{Provider, ProviderBuilder},
    signers::local::PrivateKeySigner,
    sol,
    sol_types::SolEvent,
};
use anyhow::{Context, Result};
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{error, info, warn};

// Generate Rust bindings from Solidity ABI
sol! {
    #[sol(rpc)]
    contract SonoToken {
        // Events
        event ChannelOpened(address indexed user, address indexed service, uint256 deposit);
        event ChannelToppedUp(address indexed user, address indexed service, uint256 added, uint256 total);
        event ChannelSettled(bytes32 indexed channelId, address indexed user, address indexed service, uint256 spent, uint256 refunded);

        // Views
        function balanceOf(address account) external view returns (uint256);
        function getChannel(address user, address service) external view returns (uint256 deposit, uint256 spent, uint64 nonce, uint64 expiresAt);
        function channelId(address user, address service) external pure returns (bytes32);
        function txtPerDot() external view returns (uint256);
        function quoteBuyDot(uint256 dotAmount) external view returns (uint256);
        function quoteSellDot(uint256 txtAmount) external view returns (uint256);

        // Write (service/owner calls these)
        function cooperativeClose(address user, uint256 spent, uint64 nonce, bytes sig) external;
        function setDotPrice(uint256 txtPerDot) external;
        function setTokenRate(address token, uint256 rate, uint8 tokenDec) external;
        function transfer(address to, uint256 amount) external returns (bool);
    }
}

#[derive(Debug, Clone)]
pub struct SonoConfig {
    /// eth-RPC endpoint for Asset Hub
    pub rpc_url: String,
    /// Contract address
    pub contract: Address,
    /// Service provider private key (signs state updates + settlement txs)
    pub service_key: PrivateKeySigner,
    /// Poll interval for new events (seconds)
    pub poll_interval: u64,
    /// Asset Hub Substrate RPC for price queries
    pub price_rpc_url: String,
    /// Base SONO price in USD
    pub sono_price_usd: f64,
    /// Fiat premium (0.05 = 5%)
    pub fiat_premium: f64,
    /// Price update interval in seconds
    pub price_interval: u64,
}

impl SonoConfig {
    pub fn from_env() -> Option<Self> {
        let rpc_url = std::env::var("SONO_RPC_URL").ok()?;
        let contract_hex = std::env::var("SONO_CONTRACT").ok()?;
        let key_hex = std::env::var("SONO_SERVICE_KEY").ok()?;

        let contract: Address = contract_hex.parse().ok()?;
        let service_key: PrivateKeySigner = key_hex.parse().ok()?;
        let poll_interval = std::env::var("SONO_POLL_INTERVAL")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(12); // ~1 block

        let price_rpc_url = std::env::var("ASSETHUB_RPC")
            .unwrap_or_else(|_| "wss://polkadot-asset-hub-rpc.polkadot.io".into());

        let sono_price_usd = std::env::var("SONO_PRICE_USD")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.01);

        let fiat_premium = std::env::var("SONO_FIAT_PREMIUM")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.10);

        let price_interval = std::env::var("SONO_PRICE_INTERVAL")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(300);

        Some(Self {
            rpc_url,
            contract,
            service_key,
            poll_interval,
            price_rpc_url,
            sono_price_usd,
            fiat_premium,
            price_interval,
        })
    }
}

/// Per-channel state tracked off-chain
#[derive(Debug, Clone)]
pub struct ChannelState {
    pub user: Address,
    pub deposit: U256,
    pub spent: U256,
    pub nonce: u64,
}

/// Current price info (shared across services)
#[derive(Debug, Clone)]
pub struct PriceInfo {
    /// DOT price in USD (from AssetConversion pool)
    pub dot_usd: f64,
    /// txtPerDot value sent to the contract
    pub txt_per_dot: U256,
    /// Base TXT price in USD
    pub txt_usd_base: f64,
    /// TXT price for fiat purchases (with premium)
    pub txt_usd_fiat: f64,
    /// When this was last updated
    pub updated_at: std::time::Instant,
}

impl Default for PriceInfo {
    fn default() -> Self {
        Self {
            dot_usd: 0.0,
            txt_per_dot: U256::ZERO,
            txt_usd_base: 0.01,
            txt_usd_fiat: 0.011,
            updated_at: std::time::Instant::now(),
        }
    }
}

/// SONO service — manages payment channels from the provider side
pub struct SonoService {
    config: SonoConfig,
    /// Active channels: user address → state
    channels: Arc<RwLock<std::collections::HashMap<Address, ChannelState>>>,
    /// Current price info
    pub price: Arc<RwLock<PriceInfo>>,
}

impl SonoService {
    pub fn new(config: SonoConfig) -> Self {
        let price_info = PriceInfo {
            txt_usd_base: config.sono_price_usd,
            txt_usd_fiat: config.sono_price_usd * (1.0 + config.fiat_premium),
            ..Default::default()
        };
        Self {
            config,
            channels: Arc::new(RwLock::new(std::collections::HashMap::new())),
            price: Arc::new(RwLock::new(price_info)),
        }
    }

    /// Service address (derived from the private key)
    pub fn service_address(&self) -> Address {
        self.config.service_key.address()
    }

    /// Get channel state for a user
    pub async fn get_channel(&self, user: &Address) -> Option<ChannelState> {
        self.channels.read().await.get(user).cloned()
    }

    /// Check remaining credits for a user
    pub async fn remaining(&self, user: &Address) -> U256 {
        match self.channels.read().await.get(user) {
            Some(ch) => ch.deposit.saturating_sub(ch.spent),
            None => U256::ZERO,
        }
    }

    /// Charge a user for service usage (off-chain)
    /// Returns the new cumulative spent amount, or error if insufficient
    pub async fn charge(&self, user: &Address, amount: U256) -> Result<U256> {
        let mut channels = self.channels.write().await;
        let ch = channels
            .get_mut(user)
            .context("no active channel")?;

        let new_spent = ch.spent + amount;
        if new_spent > ch.deposit {
            anyhow::bail!("insufficient channel balance");
        }

        ch.spent = new_spent;
        ch.nonce += 1;
        Ok(new_spent)
    }

    /// Sign a state update for the user (they can verify off-chain or use for dispute)
    pub async fn sign_state(&self, user: &Address) -> Result<(U256, u64, Vec<u8>)> {
        let channels = self.channels.read().await;
        let ch = channels.get(user).context("no active channel")?;

        let provider = self.make_provider().await?;
        let contract = SonoToken::new(self.config.contract, &provider);

        // Get channelId
        let cid = contract.channelId(*user, self.service_address()).call().await?;

        // Hash: keccak256(channelId, spent, nonce)
        let state_hash = alloy::primitives::keccak256(
            &[
                cid.as_slice(),
                &ch.spent.to_be_bytes::<32>(),
                &(ch.nonce as u64).to_be_bytes(),
            ]
            .concat(),
        );

        // Sign with service key
        use alloy::signers::Signer;
        let sig = self.config.service_key
            .sign_message(state_hash.as_slice())
            .await?;

        Ok((ch.spent, ch.nonce, <[u8; 65]>::from(sig).to_vec()))
    }

    /// Cooperatively close a channel on-chain
    pub async fn settle(&self, user: &Address, user_sig: Vec<u8>) -> Result<()> {
        let channels = self.channels.read().await;
        let ch = channels.get(user).context("no active channel")?;

        let provider = self.make_provider().await?;
        let contract = SonoToken::new(self.config.contract, &provider);

        let tx = contract.cooperativeClose(
            *user,
            ch.spent,
            ch.nonce,
            user_sig.into(),
        );

        let receipt = tx.send().await?.get_receipt().await?;
        info!(
            user = %user,
            spent = %ch.spent,
            tx = %receipt.transaction_hash,
            "channel settled"
        );

        // Remove from tracking
        drop(channels);
        self.channels.write().await.remove(user);

        Ok(())
    }

    /// Start the event listener loop
    pub async fn start_listener(self: Arc<Self>) -> Result<()> {
        let service_addr = self.service_address();
        info!(
            service = %service_addr,
            contract = %self.config.contract,
            "SONO listener starting"
        );

        // Sync existing channels from on-chain state
        if let Err(e) = self.sync_channels().await {
            warn!("failed to sync channels on start: {}", e);
        }

        // Poll for new events
        let mut last_block = 0u64;
        loop {
            match self.poll_events(&mut last_block).await {
                Ok(count) if count > 0 => {
                    info!(events = count, block = last_block, "processed SONO events");
                }
                Err(e) => {
                    error!("SONO event poll error: {}", e);
                }
                _ => {}
            }
            tokio::time::sleep(std::time::Duration::from_secs(self.config.poll_interval)).await;
        }
    }

    /// Start the price oracle loop
    pub async fn start_price_oracle(self: Arc<Self>, http: reqwest::Client) -> Result<()> {
        info!(
            interval = self.config.price_interval,
            sono_usd = self.config.sono_price_usd,
            fiat_premium = self.config.fiat_premium,
            "SONO price oracle starting"
        );

        loop {
            match self.update_price(&http).await {
                Ok(dot_usd) => {
                    info!(dot_usd = dot_usd, "price oracle updated");
                }
                Err(e) => {
                    warn!("price oracle update failed: {}", e);
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(self.config.price_interval)).await;
        }
    }

    /// Query AssetConversion reserves and update contract prices
    async fn update_price(&self, http: &reqwest::Client) -> Result<f64> {
        let dot_usd = self.fetch_dot_price(http).await?;
        if dot_usd <= 0.0 {
            anyhow::bail!("invalid DOT price: {}", dot_usd);
        }

        // Compute txtPerDot: how many TXT (10 decimals) per 1 DOT (18 decimals in EVM)
        // txtPerDot = (dot_usd / txt_price_usd) * 10^10
        let txt_per_dot_human = dot_usd / self.config.sono_price_usd;
        let txt_per_dot_raw = (txt_per_dot_human * 1e10) as u128;
        let txt_per_dot = U256::from(txt_per_dot_raw);

        // Update DOT price on contract
        if let Err(e) = self.set_contract_dot_price(txt_per_dot).await {
            warn!("failed to set contract DOT price: {}", e);
        }

        // Update stablecoin rates (USDC/USDT: 1 USD = 1/txt_price_usd TXT)
        // txtPerStable = (1 / txt_price_usd) * 10^10 (TXT has 10 decimals)
        let txt_per_usd = (1.0 / self.config.sono_price_usd * 1e10) as u128;
        let txt_per_stable = U256::from(txt_per_usd);

        // USDC (asset 1337, 6 decimals): 0x0000053900000000000000000000000001200000
        let usdc_addr: Address = "0x0000053900000000000000000000000001200000".parse()
            .unwrap_or_default();
        if let Err(e) = self.set_contract_token_rate(usdc_addr, txt_per_stable, 6).await {
            warn!("failed to set USDC rate: {}", e);
        }

        // USDT (asset 1984, 6 decimals): 0x000007c000000000000000000000000001200000
        let usdt_addr: Address = "0x000007c000000000000000000000000001200000".parse()
            .unwrap_or_default();
        if let Err(e) = self.set_contract_token_rate(usdt_addr, txt_per_stable, 6).await {
            warn!("failed to set USDT rate: {}", e);
        }

        // SONO (asset 50000445, 10 decimals)
        // 1 SONO = 1 TXT at base price (both $0.01, both 10 decimals)
        // txtPerSono = 1 * 10^10
        let sono_addr: Address = "0x02faf23d00000000000000000000000001200000".parse()
            .unwrap_or_default();
        let txt_per_sono = U256::from(10_000_000_000u128); // 1:1
        if let Err(e) = self.set_contract_token_rate(sono_addr, txt_per_sono, 10).await {
            warn!("failed to set SONO rate: {}", e);
        }

        // Update shared price state
        let mut price = self.price.write().await;
        price.dot_usd = dot_usd;
        price.txt_per_dot = txt_per_dot;
        price.txt_usd_base = self.config.sono_price_usd;
        price.txt_usd_fiat = self.config.sono_price_usd * (1.0 + self.config.fiat_premium);
        price.updated_at = std::time::Instant::now();

        Ok(dot_usd)
    }

    /// Fetch DOT/USD price from Asset Hub AssetConversion pool reserves
    async fn fetch_dot_price(&self, http: &reqwest::Client) -> Result<f64> {
        // Convert wss:// to https:// for HTTP JSON-RPC
        let rpc_http = self.config.price_rpc_url
            .replace("wss://", "https://")
            .replace("ws://", "http://");

        // SCALE-encoded params for AssetConversionApi_get_reserves(DOT, USDC)
        // DOT = VersionedLocation::V4(Location { parents: 1, interior: Here })
        //     = [0x04, 0x01, 0x00]
        // USDC (asset 1337) = VersionedLocation::V4(Location { parents: 0, interior: X2(PalletInstance(50), GeneralIndex(1337)) })
        //     = [0x04, 0x00, 0x02, 0x04, 0x32, 0x05, compact(1337)]
        // compact(1337) = (1337 << 2) | 0b01 = 5349 = 0x14E5 → LE: [0xE5, 0x14]
        let params_hex = "0x040100040002043205e514";

        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "state_call",
            "params": ["AssetConversionApi_get_reserves", params_hex]
        });

        let resp = http.post(&rpc_http)
            .json(&body)
            .send()
            .await
            .context("AssetConversion RPC request failed")?;

        let json: serde_json::Value = resp.json().await
            .context("AssetConversion RPC response parse failed")?;

        if let Some(error) = json.get("error") {
            // If AssetConversion API isn't available, fall back to manual price
            warn!("AssetConversion API error: {}, using fallback", error);
            return self.fetch_dot_price_fallback(http).await;
        }

        let result_hex = json.get("result")
            .and_then(|v| v.as_str())
            .context("no result in RPC response")?;

        let bytes = hex::decode(result_hex.trim_start_matches("0x"))
            .context("invalid hex in RPC result")?;

        // Decode: Option<(Balance, Balance)>
        // Option::Some = 0x01, then two u128 LE
        if bytes.is_empty() || bytes[0] == 0x00 {
            warn!("no AssetConversion pool for DOT/USDC, using fallback");
            return self.fetch_dot_price_fallback(http).await;
        }

        if bytes.len() < 33 {
            anyhow::bail!("unexpected response length: {}", bytes.len());
        }

        // Parse two u128 LE values (reserves)
        let dot_reserves = u128::from_le_bytes(bytes[1..17].try_into()?);
        let usdc_reserves = u128::from_le_bytes(bytes[17..33].try_into()?);

        if dot_reserves == 0 {
            anyhow::bail!("zero DOT reserves");
        }

        // DOT has 10 decimals on Asset Hub, USDC has 6 decimals
        // dot_price_usd = (usdc_reserves / 10^6) / (dot_reserves / 10^10)
        //               = usdc_reserves * 10^4 / dot_reserves
        let dot_price_usd = (usdc_reserves as f64 * 1e4) / dot_reserves as f64;

        info!(
            dot_reserves = dot_reserves,
            usdc_reserves = usdc_reserves,
            dot_price_usd = dot_price_usd,
            "fetched AssetConversion reserves"
        );

        Ok(dot_price_usd)
    }

    /// Fallback: use CoinGecko API for DOT price
    async fn fetch_dot_price_fallback(&self, http: &reqwest::Client) -> Result<f64> {
        let resp = http
            .get("https://api.coingecko.com/api/v3/simple/price?ids=polkadot&vs_currencies=usd")
            .send()
            .await
            .context("CoinGecko request failed")?;

        let json: serde_json::Value = resp.json().await
            .context("CoinGecko response parse failed")?;

        let price = json
            .get("polkadot")
            .and_then(|v| v.get("usd"))
            .and_then(|v| v.as_f64())
            .context("no DOT/USD price from CoinGecko")?;

        info!(dot_usd = price, "DOT price from CoinGecko fallback");
        Ok(price)
    }

    /// Call setDotPrice() on the contract
    async fn set_contract_dot_price(&self, txt_per_dot: U256) -> Result<()> {
        let provider = self.make_provider().await?;
        let contract = SonoToken::new(self.config.contract, &provider);

        // Check current on-chain price to avoid unnecessary txs
        let current = contract.txtPerDot().call().await.unwrap_or_default();

        // Only update if price changed by more than 1%
        if current > U256::ZERO {
            let diff = if txt_per_dot > current {
                txt_per_dot - current
            } else {
                current - txt_per_dot
            };
            let threshold = current / U256::from(100);
            if diff < threshold {
                return Ok(());
            }
        }

        let tx = contract.setDotPrice(txt_per_dot);
        let receipt = tx.send().await?.get_receipt().await?;
        info!(txt_per_dot = %txt_per_dot, tx = %receipt.transaction_hash, "DOT price updated");
        Ok(())
    }

    /// Call setTokenRate() on the contract for an ERC20 payment token
    async fn set_contract_token_rate(&self, token: Address, rate: U256, decimals: u8) -> Result<()> {
        let provider = self.make_provider().await?;
        let contract = SonoToken::new(self.config.contract, &provider);

        let tx = contract.setTokenRate(token, rate, decimals);
        let receipt = tx.send().await?.get_receipt().await?;
        info!(token = %token, rate = %rate, tx = %receipt.transaction_hash, "token rate updated");
        Ok(())
    }

    /// Send testnet PAS + TXT to a new user's EVM address (faucet drip)
    /// Only works when service key has sufficient balance
    pub async fn drip_testnet(&self, user_evm: Address, pas_amount: U256, txt_amount: U256) -> Result<()> {
        let provider = self.make_provider().await?;

        // Send PAS (native token)
        if pas_amount > U256::ZERO {
            let tx = alloy::rpc::types::TransactionRequest::default()
                .to(user_evm)
                .value(pas_amount);
            let pending = provider.send_transaction(tx).await?;
            let receipt = pending.get_receipt().await?;
            info!(to = %user_evm, amount = %pas_amount, tx = %receipt.transaction_hash, "dripped PAS");
        }

        // Send TXT via contract transfer
        if txt_amount > U256::ZERO {
            let contract = SonoToken::new(self.config.contract, &provider);
            let tx = contract.transfer(user_evm, txt_amount);
            let receipt = tx.send().await?.get_receipt().await?;
            info!(to = %user_evm, amount = %txt_amount, tx = %receipt.transaction_hash, "dripped TXT");
        }

        Ok(())
    }

    async fn make_provider(&self) -> Result<impl Provider> {
        let wallet = EthereumWallet::from(self.config.service_key.clone());
        let provider = ProviderBuilder::new()
            .wallet(wallet)
            .connect_http(self.config.rpc_url.parse()?);
        Ok(provider)
    }

    async fn sync_channels(&self) -> Result<()> {
        let provider = self.make_provider().await?;
        let current_block = provider.get_block_number().await?;
        let service_addr = self.service_address();

        // Scan last 1000 blocks for ChannelOpened events to this service
        let from = current_block.saturating_sub(1000);

        let filter = alloy::rpc::types::Filter::new()
            .address(self.config.contract)
            .from_block(from)
            .to_block(current_block)
            .event_signature(SonoToken::ChannelOpened::SIGNATURE_HASH);

        let logs = provider.get_logs(&filter).await?;
        let contract = SonoToken::new(self.config.contract, &provider);
        let mut synced = 0;

        for log in &logs {
            if let Ok(event) = SonoToken::ChannelOpened::decode_log(&log.inner) {
                if event.service != service_addr {
                    continue;
                }
                // Check current on-chain state (channel might be settled already)
                let ch = contract
                    .getChannel(event.user, service_addr)
                    .call()
                    .await;

                if let Ok(on_chain) = ch {
                    if on_chain.deposit > U256::ZERO {
                        let mut channels = self.channels.write().await;
                        channels.entry(event.user).or_insert_with(|| {
                            synced += 1;
                            ChannelState {
                                user: event.user,
                                deposit: on_chain.deposit,
                                spent: on_chain.spent,
                                nonce: on_chain.nonce,
                            }
                        });
                    }
                }
            }
        }

        if synced > 0 {
            info!(count = synced, "synced existing channels from chain");
        }
        Ok(())
    }

    async fn poll_events(&self, last_block: &mut u64) -> Result<usize> {
        let provider = self.make_provider().await?;
        let current_block = provider.get_block_number().await?;

        if current_block <= *last_block {
            return Ok(0);
        }

        let from = if *last_block == 0 {
            current_block.saturating_sub(100) // look back 100 blocks on first poll
        } else {
            *last_block + 1
        };

        let service_addr = self.service_address();
        let mut count = 0;

        // Query ChannelOpened events
        let filter = alloy::rpc::types::Filter::new()
            .address(self.config.contract)
            .from_block(from)
            .to_block(current_block)
            .event_signature(SonoToken::ChannelOpened::SIGNATURE_HASH);

        let logs = provider.get_logs(&filter).await?;
        for log in &logs {
            if let Ok(event) = SonoToken::ChannelOpened::decode_log(&log.inner) {
                if event.service == service_addr {
                    let mut channels = self.channels.write().await;
                    channels.insert(event.user, ChannelState {
                        user: event.user,
                        deposit: event.deposit,
                        spent: U256::ZERO,
                        nonce: 0,
                    });
                    info!(user = %event.user, deposit = %event.deposit, "channel opened");
                    count += 1;
                }
            }
        }

        // Query ChannelToppedUp events
        let filter = alloy::rpc::types::Filter::new()
            .address(self.config.contract)
            .from_block(from)
            .to_block(current_block)
            .event_signature(SonoToken::ChannelToppedUp::SIGNATURE_HASH);

        let logs = provider.get_logs(&filter).await?;
        for log in &logs {
            if let Ok(event) = SonoToken::ChannelToppedUp::decode_log(&log.inner) {
                if event.service == service_addr {
                    let mut channels = self.channels.write().await;
                    if let Some(ch) = channels.get_mut(&event.user) {
                        ch.deposit = event.total;
                        info!(user = %event.user, total = %event.total, "channel topped up");
                        count += 1;
                    }
                }
            }
        }

        // Query ChannelSettled events (remove closed channels)
        let filter = alloy::rpc::types::Filter::new()
            .address(self.config.contract)
            .from_block(from)
            .to_block(current_block)
            .event_signature(SonoToken::ChannelSettled::SIGNATURE_HASH);

        let logs = provider.get_logs(&filter).await?;
        for log in &logs {
            if let Ok(event) = SonoToken::ChannelSettled::decode_log(&log.inner) {
                if event.service == service_addr {
                    self.channels.write().await.remove(&event.user);
                    info!(user = %event.user, spent = %event.spent, "channel settled on-chain");
                    count += 1;
                }
            }
        }

        *last_block = current_block;
        Ok(count)
    }
}
