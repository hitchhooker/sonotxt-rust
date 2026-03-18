//! HD wallet derivation for Polkadot (sr25519)
//!
//! Uses hwpay crate for the underlying wallet implementation.
//! This module provides a sonotxt-compatible API.

use hwpay::wallet::polkadot::PolkadotWallet;
use secrecy::ExposeSecret;
use sha2::{Sha512, Digest};

use super::seed_manager::SeedManager;

/// Re-export decode_ss58 for backward compatibility
pub use hwpay::wallet::polkadot::decode_ss58 as decode_polkadot_address;

/// Wallet deriver wrapping hwpay's PolkadotWallet
pub struct WalletDeriver {
    inner: PolkadotWallet,
}

impl WalletDeriver {
    /// Create from raw seed bytes
    pub fn from_seed_bytes(seed_bytes: &[u8]) -> Result<Self, String> {
        let inner = PolkadotWallet::from_seed(seed_bytes)?;
        Ok(Self { inner })
    }

    /// Create from hex-encoded seed
    pub fn from_seed_hex(seed_hex: &str) -> Result<Self, String> {
        let seed_bytes = hex::decode(seed_hex.trim_start_matches("0x"))
            .map_err(|e| format!("invalid hex seed: {}", e))?;
        Self::from_seed_bytes(&seed_bytes)
    }

    /// Create from mnemonic phrase
    pub fn from_mnemonic(mnemonic: &str) -> Result<Self, String> {
        // hash mnemonic to get seed bytes
        let mut hasher = Sha512::new();
        hasher.update(b"sonotxt-master-seed:");
        hasher.update(mnemonic.as_bytes());
        let hash = hasher.finalize();
        Self::from_seed_bytes(&hash[..32])
    }

    /// Create from secure storage (TPM or encrypted file)
    pub fn from_secure_storage(password: Option<&[u8]>) -> Result<Self, String> {
        let manager = SeedManager::new();
        let seed = manager
            .load_seed(password)
            .map_err(|e| format!("failed to load seed: {}", e))?;

        Self::from_seed_bytes(seed.expose_secret())
    }

    /// Derive a Polkadot address for a user
    pub fn derive_polkadot_address(
        &self,
        user_id: &str,
        derivation_index: u32,
    ) -> String {
        self.inner.derive_address(user_id, derivation_index)
    }

    /// Sign a message with a derived keypair
    pub fn sign_with_derived(
        &self,
        user_id: &str,
        derivation_index: u32,
        message: &[u8],
    ) -> [u8; 64] {
        self.inner.sign(user_id, derivation_index, message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_derive_address() {
        let seed = [0u8; 32];
        let deriver = WalletDeriver::from_seed_bytes(&seed).unwrap();

        let addr1 = deriver.derive_polkadot_address("user1@example.com", 0);
        let addr2 = deriver.derive_polkadot_address("user1@example.com", 1);
        let addr3 = deriver.derive_polkadot_address("user2@example.com", 0);

        assert_ne!(addr1, addr2);
        assert_ne!(addr1, addr3);
        assert!(addr1.starts_with("1")); // polkadot addresses start with 1
    }
}
