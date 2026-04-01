/// ZID authentication - zafu wallet identity
///
/// clients authenticate by signing a challenge with their ed25519 ZID key.
/// no email, no password, no API key. just a signature.
///
/// auth header format:
///   Authorization: Bearer zid:<pubkey_hex>:<timestamp>:<signature_hex>
///
/// the signature covers: "sonotxt-zid-v1\n<timestamp>"
/// timestamp must be within 5 minutes of server time (replay protection).
/// the pubkey is the user's identity - accounts are created on first auth.

use axum::{
    async_trait,
    extract::FromRequestParts,
    http::request::Parts,
    RequestPartsExt,
};
use axum_extra::{
    headers::{authorization::Bearer, Authorization},
    TypedHeader,
};
use ed25519_dalek::{Signature, VerifyingKey, Verifier};
use uuid::Uuid;

use crate::{error::ApiError, AppState};

const ZID_AUTH_PREFIX: &str = "zid:";
const ZID_SIGN_DOMAIN: &str = "sonotxt-zid-v1";
const MAX_CLOCK_SKEW_SECS: i64 = 300; // 5 minutes

#[derive(Debug, Clone)]
pub struct ZidUser {
    pub account_id: Uuid,
    pub pubkey: String,
}

#[async_trait]
impl FromRequestParts<std::sync::Arc<AppState>> for ZidUser {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &std::sync::Arc<AppState>,
    ) -> Result<Self, Self::Rejection> {
        let TypedHeader(Authorization(bearer)) = parts
            .extract::<TypedHeader<Authorization<Bearer>>>()
            .await
            .map_err(|_| ApiError::InvalidApiKey)?;

        let token = bearer.token();
        if !token.starts_with(ZID_AUTH_PREFIX) {
            return Err(ApiError::InvalidApiKey);
        }

        let rest = &token[ZID_AUTH_PREFIX.len()..];
        let parts: Vec<&str> = rest.splitn(3, ':').collect();
        if parts.len() != 3 {
            return Err(ApiError::InvalidApiKey);
        }

        let pubkey_hex = parts[0];
        let timestamp_str = parts[1];
        let sig_hex = parts[2];

        // validate pubkey length (32 bytes = 64 hex chars)
        if pubkey_hex.len() != 64 {
            return Err(ApiError::InvalidApiKey);
        }

        // parse and validate timestamp
        let timestamp: i64 = timestamp_str.parse()
            .map_err(|_| ApiError::InvalidApiKey)?;
        let now = chrono::Utc::now().timestamp();
        if (now - timestamp).abs() > MAX_CLOCK_SKEW_SECS {
            return Err(ApiError::InvalidApiKey);
        }

        // verify ed25519 signature
        let pubkey_bytes: [u8; 32] = hex::decode(pubkey_hex)
            .map_err(|_| ApiError::InvalidApiKey)?
            .try_into()
            .map_err(|_| ApiError::InvalidApiKey)?;
        let sig_bytes: [u8; 64] = hex::decode(sig_hex)
            .map_err(|_| ApiError::InvalidApiKey)?
            .try_into()
            .map_err(|_| ApiError::InvalidApiKey)?;

        let verifying_key = VerifyingKey::from_bytes(&pubkey_bytes)
            .map_err(|_| ApiError::InvalidApiKey)?;
        let signature = Signature::from_bytes(&sig_bytes);

        let message = format!("{}\n{}", ZID_SIGN_DOMAIN, timestamp);
        verifying_key.verify(message.as_bytes(), &signature)
            .map_err(|_| ApiError::InvalidApiKey)?;

        // look up or create account by ZID pubkey
        let account_id = get_or_create_zid_account(&state.db, pubkey_hex).await?;

        Ok(ZidUser {
            account_id,
            pubkey: pubkey_hex.to_string(),
        })
    }
}

/// find existing account by ZID pubkey, or create one
async fn get_or_create_zid_account(
    db: &sqlx::PgPool,
    pubkey: &str,
) -> Result<Uuid, ApiError> {
    // check if account exists
    let existing: Option<(Uuid,)> = sqlx::query_as(
        "SELECT id FROM accounts WHERE zid_pubkey = $1"
    )
    .bind(pubkey)
    .fetch_optional(db)
    .await
    .map_err(|_| ApiError::InternalError)?;

    if let Some((id,)) = existing {
        return Ok(id);
    }

    // create new account
    let id = Uuid::new_v4();
    sqlx::query(
        r#"
        INSERT INTO accounts (id, zid_pubkey, created_at)
        VALUES ($1, $2, NOW())
        ON CONFLICT (zid_pubkey) DO UPDATE SET id = accounts.id
        RETURNING id
        "#
    )
    .bind(id)
    .bind(pubkey)
    .execute(db)
    .await
    .map_err(|_| ApiError::InternalError)?;

    // initialize credits
    sqlx::query(
        "INSERT INTO account_credits (account_id, balance) VALUES ($1, 0.0) ON CONFLICT DO NOTHING"
    )
    .bind(id)
    .execute(db)
    .await
    .map_err(|_| ApiError::InternalError)?;

    Ok(id)
}
