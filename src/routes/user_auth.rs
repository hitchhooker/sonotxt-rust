use axum::{
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::post,
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::{
    error::Result,
    services::{user_auth, magic_link},
    AppState,
};

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        // key-based auth
        .route("/check-identifier", post(check_identifier))
        .route("/register/key", post(register_with_key))
        .route("/login/key/start", post(start_key_login))
        .route("/login/key/complete", post(complete_key_login))
        // magic link auth
        .route("/magic-link/request", post(request_magic_link))
        .route("/magic-link/verify", post(verify_magic_link))
        // session management
        .route("/session", post(get_session))
        .route("/logout", post(logout))
}

// ============================================================================
// Key-based auth endpoints
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct CheckIdentifierRequest {
    identifier: String,
}

#[derive(Debug, Serialize)]
pub struct CheckIdentifierResponse {
    available: bool,
    public_key: String,
}

/// Check if an identifier is available and preview the derived public key
pub async fn check_identifier(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CheckIdentifierRequest>,
) -> Result<Json<CheckIdentifierResponse>> {
    if req.identifier.len() < 8 {
        return Err(crate::error::ApiError::InvalidRequestError);
    }

    let available = user_auth::is_identifier_available(&state.db, &req.identifier).await?;
    let public_key = user_auth::identifier_to_pubkey(&req.identifier);

    Ok(Json(CheckIdentifierResponse {
        available,
        public_key,
    }))
}

#[derive(Debug, Deserialize)]
pub struct RegisterKeyRequest {
    identifier: String,
}

#[derive(Debug, Serialize)]
pub struct AuthResponse {
    user_id: String,
    public_key: Option<String>,
    email: Option<String>,
    balance: f64,
    token: String,
}

/// Register with passphrase-derived key
pub async fn register_with_key(
    State(state): State<Arc<AppState>>,
    Json(req): Json<RegisterKeyRequest>,
) -> Result<Json<AuthResponse>> {
    if req.identifier.len() < 8 {
        return Err(crate::error::ApiError::InvalidRequestError);
    }

    // register user
    let user = user_auth::register_with_key(&state.db, &req.identifier).await?;

    // auto-login: create challenge and sign it
    let challenge = user_auth::start_key_login(&state.db, &req.identifier).await?;
    let signature = user_auth::sign_challenge(&req.identifier, &challenge);
    let (user, token) = user_auth::complete_key_login(
        &state.db,
        &req.identifier,
        &challenge,
        &signature,
    ).await?;

    Ok(Json(AuthResponse {
        user_id: user.id.to_string(),
        public_key: user.public_key,
        email: user.email,
        balance: user.balance,
        token,
    }))
}

#[derive(Debug, Deserialize)]
pub struct StartKeyLoginRequest {
    identifier: String,
}

#[derive(Debug, Serialize)]
pub struct ChallengeResponse {
    challenge: String,
}

/// Start key-based login - get a challenge to sign
pub async fn start_key_login(
    State(state): State<Arc<AppState>>,
    Json(req): Json<StartKeyLoginRequest>,
) -> Result<Json<ChallengeResponse>> {
    let challenge = user_auth::start_key_login(&state.db, &req.identifier).await?;
    Ok(Json(ChallengeResponse { challenge }))
}

#[derive(Debug, Deserialize)]
pub struct CompleteKeyLoginRequest {
    identifier: String,
    challenge: String,
    signature: String,
}

/// Complete key-based login with signed challenge
pub async fn complete_key_login(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CompleteKeyLoginRequest>,
) -> Result<Json<AuthResponse>> {
    let (user, token) = user_auth::complete_key_login(
        &state.db,
        &req.identifier,
        &req.challenge,
        &req.signature,
    ).await?;

    Ok(Json(AuthResponse {
        user_id: user.id.to_string(),
        public_key: user.public_key,
        email: user.email,
        balance: user.balance,
        token,
    }))
}

// ============================================================================
// Magic link auth endpoints
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct RequestMagicLinkRequest {
    email: String,
}

#[derive(Debug, Serialize)]
pub struct MessageResponse {
    message: String,
}

/// Request a magic link email
pub async fn request_magic_link(
    State(state): State<Arc<AppState>>,
    Json(req): Json<RequestMagicLinkRequest>,
) -> Result<Json<MessageResponse>> {
    let token = magic_link::request_magic_link(&state.db, &req.email).await?;

    // send email (TODO: configure base URL from env)
    let base_url = std::env::var("APP_URL").unwrap_or_else(|_| "https://app.sonotxt.com".into());
    magic_link::send_magic_link_email(&req.email, &token, &base_url).await?;

    Ok(Json(MessageResponse {
        message: "Check your email for the login link".into(),
    }))
}

#[derive(Debug, Deserialize)]
pub struct VerifyMagicLinkRequest {
    token: String,
}

/// Verify magic link and create session
pub async fn verify_magic_link(
    State(state): State<Arc<AppState>>,
    Json(req): Json<VerifyMagicLinkRequest>,
) -> Result<Json<AuthResponse>> {
    let (user, token) = magic_link::verify_magic_link(&state.db, &req.token).await?;

    Ok(Json(AuthResponse {
        user_id: user.id.to_string(),
        public_key: user.public_key,
        email: user.email,
        balance: user.balance,
        token,
    }))
}

// ============================================================================
// Session endpoints
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct SessionRequest {
    token: String,
}

#[derive(Debug, Serialize)]
pub struct UserResponse {
    user_id: String,
    public_key: Option<String>,
    email: Option<String>,
    balance: f64,
}

/// Get current user from session token
pub async fn get_session(
    State(state): State<Arc<AppState>>,
    Json(req): Json<SessionRequest>,
) -> Result<Json<UserResponse>> {
    let user = user_auth::validate_session(&state.db, &req.token).await?;

    Ok(Json(UserResponse {
        user_id: user.id.to_string(),
        public_key: user.public_key,
        email: user.email,
        balance: user.balance,
    }))
}

/// Logout - invalidate session
pub async fn logout(
    State(state): State<Arc<AppState>>,
    Json(req): Json<SessionRequest>,
) -> Result<impl IntoResponse> {
    user_auth::logout(&state.db, &req.token).await?;
    Ok(StatusCode::NO_CONTENT)
}
