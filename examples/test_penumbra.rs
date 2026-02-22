//! Test penumbra address derivation using pcli config
//! Run with: cargo run --example test_penumbra

use hwpay::wallet::penumbra::PenumbraWallet;
use uuid::Uuid;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // read spend key from pcli config
    let config_path = std::env::var("PCLI_CONFIG_PATH")
        .unwrap_or_else(|_| format!("{}/.local/share/pcli/config.toml", std::env::var("HOME").unwrap()));

    let content = std::fs::read_to_string(&config_path)?;
    let config: toml::Value = toml::from_str(&content)?;

    let spend_key = config
        .get("custody")
        .and_then(|c| c.get("spend_key"))
        .and_then(|v| v.as_str())
        .ok_or("no spend_key found")?;

    println!("using spend key from {}", config_path);

    // create wallet from bech32 spend key
    let wallet = PenumbraWallet::from_spend_key_bech32(spend_key)?;

    // derive addresses for a test account
    let test_account_id = Uuid::new_v4();
    println!("test account: {}", test_account_id);

    for i in 0..3 {
        let (addr, penumbra_index) = wallet.derive_address(&test_account_id.to_string(), i);
        println!("  derivation {}: penumbra_index={} addr={}", i, penumbra_index, addr);
    }

    // now let's derive for index 0 which we will use for pcli transfer
    let (deposit_addr, penumbra_index) = wallet.derive_address(&test_account_id.to_string(), 0);
    println!("\ndeposit address for testing:");
    println!("  account_id: {}", test_account_id);
    println!("  penumbra_index: {}", penumbra_index);
    println!("  address: {}", deposit_addr);

    println!("\nto test, run:");
    println!("  pcli tx send 100transfer/channel-2/uusdc --to {}", deposit_addr);

    Ok(())
}
