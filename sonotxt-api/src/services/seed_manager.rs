//! Secure wallet seed management
//!
//! Provides a unified interface for storing and retrieving the master wallet seed
//! using hwpay's Vault for TPM 2.0 hardware sealing or encrypted file storage.

use std::path::PathBuf;
use secrecy::{ExposeSecret, SecretBox};

use hwpay::{Vault, VaultError, SecretId, StorageMethod};

pub type SecretBytes = SecretBox<Vec<u8>>;

#[derive(Debug)]
pub enum SeedError {
    Vault(VaultError),
    NotConfigured,
    InvalidSeed(String),
}

impl std::fmt::Display for SeedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Vault(e) => write!(f, "vault error: {}", e),
            Self::NotConfigured => write!(f, "wallet seed not configured"),
            Self::InvalidSeed(s) => write!(f, "invalid seed: {}", s),
        }
    }
}

impl std::error::Error for SeedError {}

impl From<VaultError> for SeedError {
    fn from(e: VaultError) -> Self {
        Self::Vault(e)
    }
}

/// Secure wallet seed manager using hwpay vault
pub struct SeedManager {
    vault: Option<Vault>,
    password: Option<Vec<u8>>,
}

impl SeedManager {
    pub fn new() -> Self {
        Self {
            vault: None,
            password: None,
        }
    }

    /// Open vault with optional password
    fn open_vault(&self, password: Option<&[u8]>) -> Result<Vault, SeedError> {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        let data_dir = PathBuf::from(home).join(".sonotxt");
        Vault::open_at(data_dir, password).map_err(SeedError::Vault)
    }

    /// Check which storage method would be used
    pub fn storage_method(&self) -> StorageMethod {
        if hwpay::tpm::is_available() {
            StorageMethod::Tpm
        } else {
            StorageMethod::EncryptedFile
        }
    }

    /// Check if TPM is available
    pub fn tpm_available(&self) -> bool {
        hwpay::tpm::is_available()
    }

    /// Store a wallet seed securely
    pub fn store_seed(&self, seed: &[u8], password: Option<&[u8]>) -> Result<StorageMethod, SeedError> {
        if seed.len() != 32 && seed.len() != 64 {
            return Err(SeedError::InvalidSeed(format!(
                "seed must be 32 or 64 bytes, got {}",
                seed.len()
            )));
        }

        let mut vault = self.open_vault(password)?;
        vault.store(SecretId::WalletSeed, seed).map_err(SeedError::Vault)
    }

    /// Load the wallet seed from secure storage
    pub fn load_seed(&self, password: Option<&[u8]>) -> Result<SecretBytes, SeedError> {
        // check environment variable first (development only)
        if let Ok(seed_hex) = std::env::var("WALLET_SEED") {
            let seed_bytes = hex::decode(seed_hex.trim_start_matches("0x"))
                .map_err(|e| SeedError::InvalidSeed(format!("invalid hex: {}", e)))?;
            return Ok(SecretBox::new(Box::new(seed_bytes)));
        }

        let mut vault = self.open_vault(password)?;
        vault.load(SecretId::WalletSeed).map_err(SeedError::Vault)
    }

    /// Check if seed exists in storage
    pub fn seed_exists(&self) -> bool {
        if std::env::var("WALLET_SEED").is_ok() {
            return true;
        }

        if let Ok(vault) = self.open_vault(None) {
            vault.exists(&SecretId::WalletSeed)
        } else {
            false
        }
    }
}

impl Default for SeedManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_store_load_seed() {
        let dir = tempdir().unwrap();
        std::env::set_var("HOME", dir.path());

        let manager = SeedManager::new();
        let seed = [42u8; 32];

        // store with password fallback
        manager.store_seed(&seed, Some(b"testpass")).unwrap();

        // load back
        let loaded = manager.load_seed(Some(b"testpass")).unwrap();
        assert_eq!(loaded.expose_secret().as_slice(), &seed);
    }
}
