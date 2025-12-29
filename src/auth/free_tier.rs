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
use sha2::{Digest, Sha256};
use std::net::IpAddr;

use crate::{error::ApiError, AppState};
use super::api_key::AuthenticatedUser;

const FREE_TIER_DAILY_LIMIT: i32 = 1000;
const IP_HASH_SALT: &[u8] = b"sonotxt-ip-hash-salt-v1";

#[derive(Debug, Clone)]
pub enum TtsUser {
    Authenticated(AuthenticatedUser),
    FreeTier { ip_hash: String },
}

impl TtsUser {
    pub fn ip_hash(&self) -> Option<&str> {
        match self {
            TtsUser::FreeTier { ip_hash } => Some(ip_hash),
            TtsUser::Authenticated(_) => None,
        }
    }

    pub fn is_free_tier(&self) -> bool {
        matches!(self, TtsUser::FreeTier { .. })
    }
}

pub fn hash_ip(ip: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(IP_HASH_SALT);
    hasher.update(ip.as_bytes());
    let result = hasher.finalize();
    hex::encode(&result[..16])
}

fn extract_client_ip(parts: &Parts) -> Option<IpAddr> {
    // check X-Forwarded-For (haproxy adds this)
    if let Some(xff) = parts.headers.get("x-forwarded-for") {
        if let Ok(s) = xff.to_str() {
            // take first IP in chain
            if let Some(first) = s.split(',').next() {
                if let Ok(ip) = first.trim().parse() {
                    return Some(ip);
                }
            }
        }
    }

    // check X-Real-IP
    if let Some(xri) = parts.headers.get("x-real-ip") {
        if let Ok(s) = xri.to_str() {
            if let Ok(ip) = s.parse() {
                return Some(ip);
            }
        }
    }

    // fallback to connect info (works in dev)
    parts
        .extensions
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|ci| ci.0.ip())
}

#[async_trait]
impl FromRequestParts<std::sync::Arc<AppState>> for TtsUser {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &std::sync::Arc<AppState>,
    ) -> Result<Self, Self::Rejection> {
        // try bearer token first
        if let Ok(TypedHeader(Authorization(bearer))) =
            parts.extract::<TypedHeader<Authorization<Bearer>>>().await
        {
            let token = bearer.token();

            // check redis cache
            let mut redis = state.redis.clone();
            let cached: Option<String> = redis::cmd("GET")
                .arg(format!("apikey:{}", token))
                .query_async(&mut redis)
                .await
                .map_err(|_| ApiError::InternalError)?;

            if let Some(json) = cached {
                if let Ok(api_key) = serde_json::from_str::<super::api_key::ApiKey>(&json) {
                    if !api_key.revoked {
                        return Ok(TtsUser::Authenticated(AuthenticatedUser {
                            account_id: api_key.account_id,
                            api_key: api_key.key,
                        }));
                    }
                }
            }

            // check database
            let row = sqlx::query!(
                "SELECT account_id, revoked FROM api_keys WHERE key = $1",
                token
            )
            .fetch_optional(&state.db)
            .await
            .map_err(|_| ApiError::InternalError)?;

            if let Some(r) = row {
                if !r.revoked {
                    // cache it
                    let api_key = super::api_key::ApiKey {
                        key: token.to_string(),
                        account_id: r.account_id,
                        created_at: chrono::Utc::now(),
                        revoked: false,
                    };
                    let _ = redis::cmd("SETEX")
                        .arg(format!("apikey:{}", token))
                        .arg(300)
                        .arg(serde_json::to_string(&api_key).unwrap_or_default())
                        .query_async::<_, ()>(&mut redis)
                        .await;

                    return Ok(TtsUser::Authenticated(AuthenticatedUser {
                        account_id: r.account_id,
                        api_key: token.to_string(),
                    }));
                }
            }
        }

        // fall back to free tier with IP
        let ip = extract_client_ip(parts)
            .ok_or(ApiError::InvalidRequest("could not determine client ip".into()))?;

        let ip_hash = hash_ip(&ip.to_string());
        Ok(TtsUser::FreeTier { ip_hash })
    }
}

pub async fn check_free_tier_limit(
    db: &sqlx::PgPool,
    ip_hash: &str,
    chars_needed: i32,
) -> Result<i32, ApiError> {
    // get or create usage record, reset if new day
    let row = sqlx::query!(
        r#"
        INSERT INTO free_tier_usage (ip_hash, chars_used, last_reset)
        VALUES ($1, 0, CURRENT_DATE)
        ON CONFLICT (ip_hash) DO UPDATE
        SET chars_used = CASE
            WHEN free_tier_usage.last_reset < CURRENT_DATE THEN 0
            ELSE free_tier_usage.chars_used
        END,
        last_reset = CURRENT_DATE
        RETURNING chars_used
        "#,
        ip_hash
    )
    .fetch_one(db)
    .await
    .map_err(|_| ApiError::InternalError)?;

    let remaining = FREE_TIER_DAILY_LIMIT - row.chars_used;

    if remaining < chars_needed {
        return Err(ApiError::FreeTierLimitExceeded {
            remaining,
            limit: FREE_TIER_DAILY_LIMIT,
        });
    }

    Ok(remaining)
}

pub async fn consume_free_tier(
    db: &sqlx::PgPool,
    ip_hash: &str,
    chars: i32,
) -> Result<(), ApiError> {
    sqlx::query!(
        "UPDATE free_tier_usage SET chars_used = chars_used + $1 WHERE ip_hash = $2",
        chars,
        ip_hash
    )
    .execute(db)
    .await
    .map_err(|_| ApiError::InternalError)?;

    Ok(())
}
