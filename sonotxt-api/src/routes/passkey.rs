//! WebAuthn/passkey authentication endpoints

use axum::{extract::State, routing::post, Json, Router};
use redis::AsyncCommands;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use uuid::Uuid;

use crate::{
    error::{ApiError, Result},
    services::{passkey as passkey_svc, user_auth},
    AppState,
};

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/register/start", post(register_start))
        .route("/register/finish", post(register_finish))
        .route("/login/start", post(login_start))
        .route("/login/finish", post(login_finish))
}

// ============================================================================
// Request / response types
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct RegisterStartRequest {
    email: String,
}

#[derive(Debug, Serialize)]
pub struct RegisterStartResponse {
    pub options: serde_json::Value,
}

#[derive(Debug, Deserialize)]
pub struct RegisterFinishRequest {
    email: String,
    credential: serde_json::Value,
}

#[derive(Debug, Deserialize)]
pub struct LoginStartRequest {
    email: String,
}

#[derive(Debug, Serialize)]
pub struct LoginStartResponse {
    pub options: serde_json::Value,
}

#[derive(Debug, Deserialize)]
pub struct LoginFinishRequest {
    email: String,
    credential: serde_json::Value,
}

#[derive(Debug, Serialize)]
pub struct AuthResponse {
    user_id: String,
    nickname: Option<String>,
    email: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    wallet_address: Option<String>,
    balance: f64,
    token: Option<String>,
}

// ============================================================================
// Registration
// ============================================================================

/// POST /register/start - begin passkey registration
async fn register_start(
    State(state): State<Arc<AppState>>,
    Json(req): Json<RegisterStartRequest>,
) -> Result<Json<RegisterStartResponse>> {
    let email = req.email.to_lowercase().trim().to_string();
    if !email.contains('@') || email.len() < 5 {
        return Err(ApiError::InvalidRequest("invalid email".to_string()));
    }

    // Rate limit
    let mut redis = state.redis.clone();
    user_auth::check_rate_limit(&mut redis, "passkey_reg", &email).await?;

    let webauthn = passkey_svc::build_webauthn()
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    // Determine user UUID: look up existing or generate a new one
    let existing: Option<(Uuid,)> =
        sqlx::query_as("SELECT id FROM users WHERE email = $1")
            .bind(&email)
            .fetch_optional(&state.db)
            .await?;
    let user_uuid = existing.map(|(id,)| id).unwrap_or_else(Uuid::new_v4);

    // Get existing credentials (if any) to exclude during registration
    let existing_creds = passkey_svc::get_credentials_by_email(&state.db, &email).await?;

    let exclude_creds = if existing_creds.is_empty() {
        None
    } else {
        Some(existing_creds.iter().map(|c| c.cred_id().clone()).collect())
    };

    let (ccr, passkey_reg) = webauthn
        .start_passkey_registration(user_uuid, &email, &email, exclude_creds)
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    // Store registration state in Redis with 5 min TTL
    let reg_state_json =
        serde_json::to_string(&passkey_reg).map_err(|e| ApiError::Internal(e.to_string()))?;

    let redis_key = format!("passkey:reg:{}", email);
    let _: () = redis
        .set_ex(&redis_key, &reg_state_json, 300)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    let options =
        serde_json::to_value(&ccr).map_err(|e| ApiError::Internal(e.to_string()))?;

    Ok(Json(RegisterStartResponse { options }))
}

/// POST /register/finish - complete passkey registration
async fn register_finish(
    State(state): State<Arc<AppState>>,
    Json(req): Json<RegisterFinishRequest>,
) -> Result<Json<AuthResponse>> {
    let email = req.email.to_lowercase().trim().to_string();

    let mut redis = state.redis.clone();

    // Retrieve registration state from Redis
    let redis_key = format!("passkey:reg:{}", email);
    let reg_state_json: Option<String> = redis
        .get(&redis_key)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    let reg_state_json = reg_state_json.ok_or(ApiError::InvalidRequest(
        "registration session expired or not found".to_string(),
    ))?;

    // Delete the key so it can't be reused
    let _: () = redis
        .del(&redis_key)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    let passkey_reg: webauthn_rs::prelude::PasskeyRegistration =
        serde_json::from_str(&reg_state_json).map_err(|e| ApiError::Internal(e.to_string()))?;

    let webauthn = passkey_svc::build_webauthn()
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    // Parse the client credential response
    let reg_response: webauthn_rs::prelude::RegisterPublicKeyCredential =
        serde_json::from_value(req.credential)
            .map_err(|e| ApiError::InvalidRequest(e.to_string()))?;

    let passkey = webauthn
        .finish_passkey_registration(&reg_response, &passkey_reg)
        .map_err(|e| ApiError::InvalidRequest(format!("registration verification failed: {}", e)))?;

    // Upsert user
    let user: (Uuid, Option<String>, Option<String>, Option<String>, f64) = sqlx::query_as(
        r#"
        INSERT INTO users (email, email_verified) VALUES ($1, TRUE)
        ON CONFLICT (email) DO UPDATE SET email_verified = TRUE, last_login = NOW()
        RETURNING id, nickname, email, public_key, balance::float8
        "#,
    )
    .bind(&email)
    .fetch_one(&state.db)
    .await?;

    let user_id = user.0;

    // Store passkey credential
    passkey_svc::store_credential(&state.db, user_id, &passkey).await?;

    // Create session
    let token = user_auth::generate_session_token();
    let token_hash = user_auth::hash_token(&token);
    let expires = chrono::Utc::now() + chrono::Duration::days(30);

    sqlx::query(
        r#"
        INSERT INTO sessions (user_id, token_hash, expires_at)
        VALUES ($1, $2, $3)
        "#,
    )
    .bind(user_id)
    .bind(&token_hash)
    .bind(expires)
    .execute(&state.db)
    .await?;

    Ok(Json(AuthResponse {
        user_id: user_id.to_string(),
        nickname: user.1,
        email: user.2,
        wallet_address: None,
        balance: user.4,
        token: Some(token),
    }))
}

// ============================================================================
// Login
// ============================================================================

/// POST /login/start - begin passkey login
async fn login_start(
    State(state): State<Arc<AppState>>,
    Json(req): Json<LoginStartRequest>,
) -> Result<Json<LoginStartResponse>> {
    let email = req.email.to_lowercase().trim().to_string();

    let mut redis = state.redis.clone();
    user_auth::check_rate_limit(&mut redis, "passkey_login", &email).await?;

    let existing_creds = passkey_svc::get_credentials_by_email(&state.db, &email).await?;
    if existing_creds.is_empty() {
        return Err(ApiError::InvalidCredentials);
    }

    let webauthn = passkey_svc::build_webauthn()
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    let (rcr, passkey_auth) = webauthn
        .start_passkey_authentication(&existing_creds)
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    // Store auth state in Redis with 5 min TTL
    let auth_state_json =
        serde_json::to_string(&passkey_auth).map_err(|e| ApiError::Internal(e.to_string()))?;

    // Use credential ID hex from the first allowed credential as part of the key
    let redis_key = format!("passkey:login:{}", email);
    let _: () = redis
        .set_ex(&redis_key, &auth_state_json, 300)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    let options =
        serde_json::to_value(&rcr).map_err(|e| ApiError::Internal(e.to_string()))?;

    Ok(Json(LoginStartResponse { options }))
}

/// POST /login/finish - complete passkey login
async fn login_finish(
    State(state): State<Arc<AppState>>,
    Json(req): Json<LoginFinishRequest>,
) -> Result<Json<AuthResponse>> {
    let email = req.email.to_lowercase().trim().to_string();

    let mut redis = state.redis.clone();

    // Retrieve auth state from Redis
    let redis_key = format!("passkey:login:{}", email);
    let auth_state_json: Option<String> = redis
        .get(&redis_key)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    let auth_state_json = auth_state_json.ok_or(ApiError::InvalidRequest(
        "login session expired or not found".to_string(),
    ))?;

    // Delete the key
    let _: () = redis
        .del(&redis_key)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    let passkey_auth: webauthn_rs::prelude::PasskeyAuthentication =
        serde_json::from_str(&auth_state_json).map_err(|e| ApiError::Internal(e.to_string()))?;

    let webauthn = passkey_svc::build_webauthn()
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    // Parse the client credential response
    let auth_response: webauthn_rs::prelude::PublicKeyCredential =
        serde_json::from_value(req.credential)
            .map_err(|e| ApiError::InvalidRequest(e.to_string()))?;

    let auth_result = webauthn
        .finish_passkey_authentication(&auth_response, &passkey_auth)
        .map_err(|_e| ApiError::InvalidCredentials)?;

    // Update credential counter in DB
    let mut creds = passkey_svc::get_credentials_by_email(&state.db, &email).await?;
    for cred in &mut creds {
        if let Some(true) = cred.update_credential(&auth_result) {
            passkey_svc::update_credential(&state.db, cred).await?;
        }
    }

    // Get the user
    let user: (Uuid, Option<String>, Option<String>, Option<String>, f64) = sqlx::query_as(
        r#"
        SELECT id, nickname, email, public_key, balance::float8
        FROM users WHERE email = $1
        "#,
    )
    .bind(&email)
    .fetch_one(&state.db)
    .await?;

    let user_id = user.0;

    // Update last_login
    sqlx::query("UPDATE users SET last_login = NOW() WHERE id = $1")
        .bind(user_id)
        .execute(&state.db)
        .await?;

    // Create session
    let token = user_auth::generate_session_token();
    let token_hash = user_auth::hash_token(&token);
    let expires = chrono::Utc::now() + chrono::Duration::days(30);

    sqlx::query(
        r#"
        INSERT INTO sessions (user_id, token_hash, expires_at)
        VALUES ($1, $2, $3)
        "#,
    )
    .bind(user_id)
    .bind(&token_hash)
    .bind(expires)
    .execute(&state.db)
    .await?;

    // Clear rate limit on success
    user_auth::clear_rate_limit(&mut redis, "passkey_login", &email).await;

    Ok(Json(AuthResponse {
        user_id: user_id.to_string(),
        nickname: user.1,
        email: user.2,
        wallet_address: None,
        balance: user.4,
        token: Some(token),
    }))
}
