use axum::{
    extract::{State, Query},
    http::HeaderMap,
    routing::{get, post},
    Json, Router,
};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::{error::ApiError, AppState};

type HmacSha256 = Hmac<Sha256>;

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/embed/tts", post(embed_tts))
        .route("/embed/user-tts", post(user_embed_tts))
        .route("/embed/status", get(embed_status))
        .route("/embed/verify", get(verify_domain))
}

#[derive(Debug, Deserialize)]
pub struct EmbedTtsRequest {
    text: String,
    voice: Option<String>,
    sig: String,
}

#[derive(Debug, Serialize)]
pub struct EmbedTtsResponse {
    job_id: String,
    status: String,
}

/// verify HMAC(secret, domain) == sig
fn verify_embed_signature(secret: &str, domain: &str, sig: &str) -> bool {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
        .expect("hmac accepts any key length");
    mac.update(domain.as_bytes());

    // sig is hex encoded
    let sig_bytes = match hex::decode(sig) {
        Ok(b) => b,
        Err(_) => return false,
    };

    mac.verify_slice(&sig_bytes).is_ok()
}

/// generate embed signature for a domain (admin use)
pub fn generate_embed_signature(secret: &str, domain: &str) -> String {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
        .expect("hmac accepts any key length");
    mac.update(domain.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// extract domain from origin header
fn extract_domain(headers: &HeaderMap) -> Option<String> {
    headers
        .get("origin")
        .or_else(|| headers.get("referer"))
        .and_then(|v| v.to_str().ok())
        .and_then(|s| {
            url::Url::parse(s).ok().and_then(|u| u.host_str().map(|h| h.to_string()))
        })
}

async fn embed_tts(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<EmbedTtsRequest>,
) -> Result<Json<EmbedTtsResponse>, ApiError> {
    let secret = state.config.embed_secret.as_ref()
        .ok_or_else(|| ApiError::Internal("embed not configured".into()))?;

    let domain = extract_domain(&headers)
        .ok_or_else(|| ApiError::InvalidRequest("missing origin".into()))?;

    // verify signature
    if !verify_embed_signature(secret, &domain, &req.sig) {
        return Err(ApiError::Unauthorized);
    }

    let text = req.text.trim();
    if text.is_empty() {
        return Err(ApiError::InvalidRequest("empty text".into()));
    }
    if text.len() > 5000 {
        return Err(ApiError::ContentTooLarge);
    }

    // upsert embed site record
    let site: (uuid::Uuid, Option<i32>, Option<bool>) = sqlx::query_as(
        r#"
        INSERT INTO embed_sites (domain)
        VALUES ($1)
        ON CONFLICT (domain) DO UPDATE SET last_used_at = now()
        RETURNING id, daily_char_limit, enabled
        "#,
    )
    .bind(&domain)
    .fetch_one(&state.db)
    .await
    .map_err(|e: sqlx::Error| ApiError::Internal(e.to_string()))?;

    if !site.2.unwrap_or(true) {
        return Err(ApiError::InvalidRequest("domain disabled".into()));
    }
    let daily_char_limit = site.1;

    // check daily limit via redis
    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let key = format!("embed:{}:{}", domain, today);

    let mut redis = state.redis.clone();
    let current: i64 = redis::cmd("GET")
        .arg(&key)
        .query_async(&mut redis)
        .await
        .unwrap_or(0);

    let limit = daily_char_limit.unwrap_or(state.config.embed_daily_limit);
    if current + text.len() as i64 > limit as i64 {
        return Err(ApiError::FreeTierLimitExceeded {
            remaining: (limit as i64 - current) as i32,
            limit,
        });
    }

    // increment usage
    let _: () = redis::cmd("INCRBY")
        .arg(&key)
        .arg(text.len())
        .query_async(&mut redis)
        .await
        .unwrap_or(());
    let _: () = redis::cmd("EXPIRE")
        .arg(&key)
        .arg(86400 * 2) // 2 days ttl
        .query_async(&mut redis)
        .await
        .unwrap_or(());

    // update site stats
    let _ = sqlx::query(
        r#"
        UPDATE embed_sites
        SET total_requests = total_requests + 1,
            total_chars = total_chars + $1
        WHERE domain = $2
        "#,
    )
    .bind(text.len() as i64)
    .bind(&domain)
    .execute(&state.db)
    .await;

    // create job
    let job_id = uuid::Uuid::new_v4().to_string();
    let voice = req.voice.unwrap_or_else(|| "af_bella".to_string());

    sqlx::query(
        r#"
        INSERT INTO jobs (id, text_content, voice, char_count, embed_domain)
        VALUES ($1, $2, $3, $4, $5)
        "#,
    )
    .bind(&job_id)
    .bind(text)
    .bind(&voice)
    .bind(text.len() as i32)
    .bind(&domain)
    .execute(&state.db)
    .await
    .map_err(|e: sqlx::Error| ApiError::Internal(e.to_string()))?;

    Ok(Json(EmbedTtsResponse {
        job_id,
        status: "queued".into(),
    }))
}

/// user-signed embed - user signs domain with their ed25519 key
#[derive(Debug, Deserialize)]
pub struct UserEmbedRequest {
    text: String,
    voice: Option<String>,
    user: String,      // nickname
    sig: String,       // ed25519 signature of domain
}

async fn user_embed_tts(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<UserEmbedRequest>,
) -> Result<Json<EmbedTtsResponse>, ApiError> {
    let domain = extract_domain(&headers)
        .ok_or_else(|| ApiError::InvalidRequest("missing origin".into()))?;

    // get user's public key from db
    let user: Option<(uuid::Uuid, String, f64)> = sqlx::query_as(
        "SELECT id, public_key, balance FROM users WHERE LOWER(nickname) = LOWER($1)"
    )
    .bind(&req.user)
    .fetch_optional(&state.db)
    .await
    .map_err(|e: sqlx::Error| ApiError::Internal(e.to_string()))?;

    let (user_id, pubkey_hex, balance) = user.ok_or(ApiError::NotFound)?;

    // verify ed25519 signature
    let pubkey_bytes = hex::decode(&pubkey_hex)
        .map_err(|_| ApiError::InvalidRequest("invalid pubkey".into()))?;
    let pubkey = VerifyingKey::from_bytes(
        pubkey_bytes.as_slice().try_into()
            .map_err(|_| ApiError::InvalidRequest("invalid pubkey length".into()))?
    ).map_err(|_| ApiError::InvalidRequest("invalid pubkey".into()))?;

    let sig_bytes = hex::decode(&req.sig)
        .map_err(|_| ApiError::InvalidRequest("invalid signature".into()))?;
    let signature = Signature::from_bytes(
        sig_bytes.as_slice().try_into()
            .map_err(|_| ApiError::InvalidRequest("invalid signature length".into()))?
    );

    pubkey.verify(domain.as_bytes(), &signature)
        .map_err(|_| ApiError::Unauthorized)?;

    // check text
    let text = req.text.trim();
    if text.is_empty() {
        return Err(ApiError::InvalidRequest("empty text".into()));
    }
    if text.len() > 10000 {
        return Err(ApiError::ContentTooLarge);
    }

    // check user balance (cost estimate)
    let cost = text.len() as f64 * state.config.cost_per_char;
    if balance < cost {
        return Err(ApiError::InsufficientBalance);
    }

    // create job linked to user
    let job_id = uuid::Uuid::new_v4().to_string();
    let voice = req.voice.unwrap_or_else(|| "af_bella".to_string());

    sqlx::query(
        r#"
        INSERT INTO jobs (id, text_content, voice, char_count, embed_domain, user_id)
        VALUES ($1, $2, $3, $4, $5, $6)
        "#,
    )
    .bind(&job_id)
    .bind(text)
    .bind(&voice)
    .bind(text.len() as i32)
    .bind(&domain)
    .bind(user_id)
    .execute(&state.db)
    .await
    .map_err(|e: sqlx::Error| ApiError::Internal(e.to_string()))?;

    Ok(Json(EmbedTtsResponse {
        job_id,
        status: "queued".into(),
    }))
}

#[derive(Debug, Deserialize)]
pub struct StatusQuery {
    job_id: String,
}

async fn embed_status(
    State(state): State<Arc<AppState>>,
    Query(q): Query<StatusQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let job: Option<(String, Option<String>, Option<f64>, Option<String>)> = sqlx::query_as(
        r#"
        SELECT status, audio_url, duration_seconds, error_message
        FROM jobs
        WHERE id = $1
        "#,
    )
    .bind(&q.job_id)
    .fetch_optional(&state.db)
    .await
    .map_err(|e: sqlx::Error| ApiError::Internal(e.to_string()))?;

    let job = job.ok_or(ApiError::NotFound)?;

    Ok(Json(serde_json::json!({
        "status": job.0,
        "url": job.1,
        "duration": job.2,
        "error": job.3
    })))
}

#[derive(Debug, Deserialize)]
pub struct VerifyQuery {
    domain: String,
    sig: String,
}

async fn verify_domain(
    State(state): State<Arc<AppState>>,
    Query(q): Query<VerifyQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let secret = state.config.embed_secret.as_ref()
        .ok_or_else(|| ApiError::Internal("embed not configured".into()))?;

    let valid = verify_embed_signature(secret, &q.domain, &q.sig);

    Ok(Json(serde_json::json!({
        "domain": q.domain,
        "valid": valid
    })))
}
