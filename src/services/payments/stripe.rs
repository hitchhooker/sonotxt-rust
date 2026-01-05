use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::{ApiError, Config, Result};

pub struct StripeService {
    secret_key: Option<String>,
    config: Config,
}

#[derive(Serialize)]
struct CreateCheckoutSessionRequest {
    mode: String,
    success_url: String,
    cancel_url: String,
    client_reference_id: String,
    line_items: Vec<LineItem>,
}

#[derive(Serialize)]
struct LineItem {
    price_data: PriceData,
    quantity: u32,
}

#[derive(Serialize)]
struct PriceData {
    currency: String,
    unit_amount: i64,
    product_data: ProductData,
}

#[derive(Serialize)]
struct ProductData {
    name: String,
}

#[derive(Deserialize)]
struct CheckoutSessionResponse {
    url: Option<String>,
    id: String,
}

#[derive(Deserialize)]
struct StripeEvent {
    #[serde(rename = "type")]
    event_type: String,
    data: StripeEventData,
}

#[derive(Deserialize)]
struct StripeEventData {
    object: serde_json::Value,
}

/// Supported currencies
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Currency {
    #[default]
    Eur,
    Usd,
}

impl Currency {
    pub fn as_str(&self) -> &'static str {
        match self {
            Currency::Eur => "eur",
            Currency::Usd => "usd",
        }
    }

    pub fn symbol(&self) -> &'static str {
        match self {
            Currency::Eur => "€",
            Currency::Usd => "$",
        }
    }
}

impl StripeService {
    pub fn new(config: &Config) -> Self {
        Self {
            secret_key: config.stripe_secret_key.clone(),
            config: config.clone(),
        }
    }

    /// create a checkout session for credit purchase (EUR default for EU)
    pub async fn create_checkout_session(
        &self,
        account_id: Uuid,
        amount: f64,
    ) -> Result<String> {
        self.create_checkout_session_with_currency(account_id, amount, Currency::Eur).await
    }

    /// create a checkout session with specific currency
    pub async fn create_checkout_session_with_currency(
        &self,
        account_id: Uuid,
        amount: f64,
        currency: Currency,
    ) -> Result<String> {
        let secret_key = self
            .secret_key
            .as_ref()
            .ok_or_else(|| ApiError::Internal("stripe not configured".into()))?;

        let amount_cents = (amount * 100.0) as i64;

        let request = CreateCheckoutSessionRequest {
            mode: "payment".into(),
            success_url: format!(
                "{}/billing/success?session_id={{CHECKOUT_SESSION_ID}}",
                self.config.app_url
            ),
            cancel_url: format!("{}/billing", self.config.app_url),
            client_reference_id: account_id.to_string(),
            line_items: vec![LineItem {
                price_data: PriceData {
                    currency: currency.as_str().into(),
                    unit_amount: amount_cents,
                    product_data: ProductData {
                        name: format!("{}{:.2} SonoTxt Credits", currency.symbol(), amount),
                    },
                },
                quantity: 1,
            }],
        };

        let client = reqwest::Client::new();
        let response = client
            .post("https://api.stripe.com/v1/checkout/sessions")
            .basic_auth(secret_key, None::<&str>)
            .form(&[
                ("mode", &request.mode),
                ("success_url", &request.success_url),
                ("cancel_url", &request.cancel_url),
                ("client_reference_id", &request.client_reference_id),
                (
                    "line_items[0][price_data][currency]",
                    &request.line_items[0].price_data.currency,
                ),
                (
                    "line_items[0][price_data][unit_amount]",
                    &request.line_items[0].price_data.unit_amount.to_string(),
                ),
                (
                    "line_items[0][price_data][product_data][name]",
                    &request.line_items[0].price_data.product_data.name,
                ),
                (
                    "line_items[0][quantity]",
                    &request.line_items[0].quantity.to_string(),
                ),
            ])
            .send()
            .await
            .map_err(|e| ApiError::Internal(format!("stripe request: {}", e)))?;

        if !response.status().is_success() {
            let error = response.text().await.unwrap_or_default();
            return Err(ApiError::Internal(format!("stripe error: {}", error)));
        }

        let session: CheckoutSessionResponse = response
            .json()
            .await
            .map_err(|e| ApiError::Internal(format!("stripe parse: {}", e)))?;

        session
            .url
            .ok_or_else(|| ApiError::Internal("no checkout url".into()))
    }

    /// verify and process webhook event
    pub async fn handle_webhook(
        db: &PgPool,
        config: &Config,
        payload: &str,
        signature: &str,
    ) -> Result<()> {
        let webhook_secret = config
            .stripe_webhook_secret
            .as_ref()
            .ok_or_else(|| ApiError::Internal("webhook secret not configured".into()))?;

        // verify signature
        if !Self::verify_signature(payload, signature, webhook_secret) {
            return Err(ApiError::InvalidRequest("invalid signature".into()));
        }

        let event: StripeEvent = serde_json::from_str(payload)
            .map_err(|e| ApiError::InvalidRequest(format!("invalid event: {}", e)))?;

        if event.event_type == "checkout.session.completed" {
            Self::handle_checkout_complete(db, &event.data.object).await?;
        }

        Ok(())
    }

    fn verify_signature(payload: &str, signature: &str, secret: &str) -> bool {
        // parse stripe signature header
        let mut timestamp: Option<i64> = None;
        let mut signatures = Vec::new();

        for part in signature.split(',') {
            let mut kv = part.splitn(2, '=');
            match (kv.next(), kv.next()) {
                (Some("t"), Some(ts)) => timestamp = ts.parse().ok(),
                (Some("v1"), Some(sig)) => signatures.push(sig.to_string()),
                _ => {}
            }
        }

        let ts = match timestamp {
            Some(ts) => ts,
            None => return false,
        };

        // construct signed payload
        let signed_payload = format!("{}.{}", ts, payload);

        // compute expected signature
        use hmac::{Hmac, Mac};
        use sha2::Sha256;

        type HmacSha256 = Hmac<Sha256>;

        let mut mac = match HmacSha256::new_from_slice(secret.as_bytes()) {
            Ok(m) => m,
            Err(_) => return false,
        };
        mac.update(signed_payload.as_bytes());
        let expected = hex::encode(mac.finalize().into_bytes());

        signatures.iter().any(|sig| sig == &expected)
    }

    async fn handle_checkout_complete(db: &PgPool, session: &serde_json::Value) -> Result<()> {
        let account_id: Uuid = session
            .get("client_reference_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ApiError::Internal("no client reference id".into()))?
            .parse()
            .map_err(|_| ApiError::Internal("invalid account id".into()))?;

        let amount_cents = session
            .get("amount_total")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let amount = amount_cents as f64 / 100.0;

        // get currency from session (default EUR for EU company)
        let currency = session
            .get("currency")
            .and_then(|v| v.as_str())
            .unwrap_or("eur")
            .to_uppercase();

        let payment_id = session
            .get("payment_intent")
            .or_else(|| session.get("id"))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();

        // check for duplicate
        let exists: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM deposits WHERE tx_hash = $1)")
                .bind(&payment_id)
                .fetch_one(db)
                .await
                .unwrap_or(false);

        if exists {
            return Ok(());
        }

        // record deposit
        let deposit_id = Uuid::new_v4();
        sqlx::query(
            r#"
            INSERT INTO deposits (id, account_id, chain, tx_hash, asset, amount, status, confirmations)
            VALUES ($1, $2, 'stripe', $3, $4, $5, 'confirmed', 1)
            "#,
        )
        .bind(deposit_id)
        .bind(account_id)
        .bind(&payment_id)
        .bind(&currency)
        .bind(amount)
        .execute(db)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

        // credit account (amount is in the original currency, stored as credits)
        super::credit_deposit(db, deposit_id, account_id, amount, "stripe", &payment_id).await?;

        Ok(())
    }
}
