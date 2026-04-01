/// Zcash shielded payment scanner for ZID-based deposits.
///
/// Watches a single shielded address via z_listreceivedbyaddress.
/// Memos with "tts:<zid_pubkey>" prefix identify the paying user.
/// Credits are added to the user's account based on ZEC amount.
///
/// Rate: configurable via ZEC_PER_DOLLAR env (default: ~$30/ZEC).
/// 0.01 ZEC = ~$0.30 = ~187 minutes of TTS at $0.0016/min.

use std::collections::HashSet;
use std::time::Duration;

use serde::Deserialize;
use sqlx::PgPool;
use tracing::{info, warn, error, debug};
use uuid::Uuid;

const MEMO_PREFIX: &str = "tts:";

#[derive(Clone)]
pub struct ZcashScannerConfig {
    pub zebrad_rpc: String,
    pub deposit_address: String,
    pub poll_interval_secs: u64,
    /// ZEC per USD (for converting payment to credit balance)
    pub zec_per_dollar: f64,
    /// cost per minute of TTS in USD
    pub cost_per_minute: f64,
}

pub struct ZcashScanner {
    config: ZcashScannerConfig,
    client: reqwest::Client,
    seen_txids: HashSet<String>,
}

impl ZcashScanner {
    pub fn new(config: ZcashScannerConfig) -> Self {
        Self {
            config,
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .expect("failed to build http client"),
            seen_txids: HashSet::new(),
        }
    }

    /// run the scanner loop, crediting accounts as payments arrive
    pub async fn run(&mut self, db: PgPool) {
        info!(
            "starting zcash payment scanner for {} (poll {}s)",
            self.config.deposit_address, self.config.poll_interval_secs
        );

        loop {
            if let Err(e) = self.scan(&db).await {
                error!("zcash scan error: {}", e);
            }
            tokio::time::sleep(Duration::from_secs(self.config.poll_interval_secs)).await;
        }
    }

    async fn scan(&mut self, db: &PgPool) -> anyhow::Result<()> {
        let payments = self.list_received().await?;

        for payment in &payments {
            if payment.confirmations < 1 {
                continue; // skip 0-conf for now
            }

            if self.seen_txids.contains(&payment.txid) {
                continue;
            }

            let memo = decode_memo(&payment.memo);
            if !memo.starts_with(MEMO_PREFIX) {
                continue;
            }

            let zid_pubkey = memo.trim_start_matches(MEMO_PREFIX).trim();
            if zid_pubkey.is_empty() || zid_pubkey.len() < 32 {
                warn!("invalid zid pubkey in memo: {}", memo);
                continue;
            }

            // find account by ZID pubkey
            let account: Option<(Uuid,)> = sqlx::query_as(
                "SELECT id FROM accounts WHERE zid_pubkey = $1"
            )
            .bind(zid_pubkey)
            .fetch_optional(db)
            .await?;

            let account_id = match account {
                Some((id,)) => id,
                None => {
                    // auto-create account for this ZID
                    let id = Uuid::new_v4();
                    sqlx::query(
                        "INSERT INTO accounts (id, zid_pubkey, created_at) VALUES ($1, $2, NOW()) ON CONFLICT (zid_pubkey) DO NOTHING"
                    )
                    .bind(id)
                    .bind(zid_pubkey)
                    .execute(db)
                    .await?;
                    sqlx::query(
                        "INSERT INTO account_credits (account_id, balance) VALUES ($1, 0.0) ON CONFLICT DO NOTHING"
                    )
                    .bind(id)
                    .execute(db)
                    .await?;
                    info!("auto-created account {} for zid {}", id, &zid_pubkey[..16]);
                    id
                }
            };

            // convert ZEC to USD credit
            let usd_value = payment.amount / self.config.zec_per_dollar;

            // credit the account
            sqlx::query(
                "UPDATE account_credits SET balance = balance + $1, updated_at = NOW() WHERE account_id = $2"
            )
            .bind(usd_value)
            .bind(account_id)
            .execute(db)
            .await?;

            // record the transaction
            sqlx::query(
                r#"
                INSERT INTO transactions (account_id, amount, type, description, chain, tx_hash)
                VALUES ($1, $2, 'purchase', $3, 'zcash', $4)
                ON CONFLICT DO NOTHING
                "#
            )
            .bind(account_id)
            .bind(usd_value)
            .bind(format!("{:.4} ZEC shielded deposit", payment.amount))
            .bind(&payment.txid)
            .execute(db)
            .await?;

            self.seen_txids.insert(payment.txid.clone());

            info!(
                "credited ${:.4} ({:.4} ZEC) to zid {}... (tx {})",
                usd_value,
                payment.amount,
                &zid_pubkey[..16],
                &payment.txid[..16]
            );
        }

        Ok(())
    }

    async fn list_received(&self) -> anyhow::Result<Vec<ReceivedPayment>> {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "z_listreceivedbyaddress",
            "params": [&self.config.deposit_address, 0],
        });

        let res = self.client.post(&self.config.zebrad_rpc)
            .json(&body)
            .send()
            .await?;

        let rpc: RpcResponse<Vec<ReceivedPayment>> = res.json().await?;

        if let Some(err) = rpc.error {
            anyhow::bail!("rpc error {}: {}", err.code, err.message);
        }

        Ok(rpc.result.unwrap_or_default())
    }
}

fn decode_memo(hex: &str) -> String {
    let bytes = match hex::decode(hex) {
        Ok(b) => b,
        Err(_) => return String::new(),
    };
    // strip trailing null bytes (zcash pads memos to 512 bytes)
    let trimmed: Vec<u8> = bytes.into_iter().take_while(|&b| b != 0).collect();
    String::from_utf8(trimmed).unwrap_or_default()
}

#[derive(Deserialize)]
struct ReceivedPayment {
    txid: String,
    amount: f64,
    memo: String,
    #[serde(default)]
    confirmations: u32,
}

#[derive(Deserialize)]
struct RpcResponse<T> {
    result: Option<T>,
    error: Option<RpcError>,
}

#[derive(Deserialize)]
struct RpcError {
    code: i64,
    message: String,
}
