//! SONO payment channel service
//!
//! Interacts with the SonoToken contract on Asset Hub via eth-RPC.
//! The API server acts as the service provider side of payment channels:
//!
//! 1. Watches for ChannelOpened events → grants user off-chain credits
//! 2. Tracks cumulative spend per channel, signs state updates
//! 3. Settles channels via cooperativeClose when user requests it

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

        // Write (service calls these)
        function cooperativeClose(address user, uint256 spent, uint64 nonce, bytes sig) external;
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

        Some(Self {
            rpc_url,
            contract,
            service_key,
            poll_interval,
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

/// SONO service — manages payment channels from the provider side
pub struct SonoService {
    config: SonoConfig,
    /// Active channels: user address → state
    channels: Arc<RwLock<std::collections::HashMap<Address, ChannelState>>>,
}

impl SonoService {
    pub fn new(config: SonoConfig) -> Self {
        Self {
            config,
            channels: Arc::new(RwLock::new(std::collections::HashMap::new())),
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

    async fn make_provider(&self) -> Result<impl Provider> {
        let wallet = EthereumWallet::from(self.config.service_key.clone());
        let provider = ProviderBuilder::new()
            .wallet(wallet)
            .connect_http(self.config.rpc_url.parse()?);
        Ok(provider)
    }

    async fn sync_channels(&self) -> Result<()> {
        // TODO: query on-chain for existing channels to this service
        // For now, channels are populated from events only
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
