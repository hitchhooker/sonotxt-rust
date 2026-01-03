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
    error::{ApiError, Result},
    services::{magic_link, user_auth},
    AppState,
};

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        // nickname + client-derived key auth
        .route("/check-nickname", post(check_nickname))
        .route("/register", post(register))
        .route("/challenge", post(get_challenge))
        .route("/verify", post(verify))
        // magic link auth
        .route("/magic-link/request", post(request_magic_link))
        .route("/magic-link/verify", post(verify_magic_link))
        // session management
        .route("/session", post(get_session))
        .route("/logout", post(logout))
}

// ============================================================================
// Nickname + client-derived key auth endpoints
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct CheckNicknameRequest {
    nickname: String,
}

#[derive(Debug, Serialize)]
pub struct CheckNicknameResponse {
    available: bool,
}

/// Check if a nickname is available
pub async fn check_nickname(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CheckNicknameRequest>,
) -> Result<Json<CheckNicknameResponse>> {
    if let Err(e) = user_auth::validate_nickname(&req.nickname) {
        return Err(crate::error::ApiError::InvalidRequest(e.to_string()));
    }

    let available = user_auth::is_nickname_available(&state.db, &req.nickname).await?;

    Ok(Json(CheckNicknameResponse { available }))
}

#[derive(Debug, Deserialize)]
pub struct RegisterRequest {
    nickname: String,
    public_key: String,
    email: Option<String>,
    recovery_share: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct AuthResponse {
    user_id: String,
    nickname: Option<String>,
    email: Option<String>,
    balance: f64,
    token: Option<String>,
}

/// Register with nickname + public_key (derived client-side)
pub async fn register(
    State(state): State<Arc<AppState>>,
    Json(req): Json<RegisterRequest>,
) -> Result<Json<AuthResponse>> {
    // rate limit by public key to prevent spam registrations
    let mut redis = state.redis.clone();
    user_auth::check_rate_limit(&mut redis, "register", &req.public_key).await?;

    let result = user_auth::register_with_pubkey(
        &state.db,
        &req.nickname,
        &req.public_key,
        req.email.as_deref(),
        req.recovery_share.as_deref(),
    ).await;

    match result {
        Ok(user) => {
            user_auth::clear_rate_limit(&mut redis, "register", &req.public_key).await;

            Ok(Json(AuthResponse {
                user_id: user.id.to_string(),
                nickname: user.nickname,
                email: user.email,
                balance: user.balance,
                token: None, // User needs to login after registration
            }))
        }
        Err(e) => Err(e),
    }
}

#[derive(Debug, Deserialize)]
pub struct ChallengeRequest {
    nickname: String,
}

#[derive(Debug, Serialize)]
pub struct ChallengeResponse {
    challenge: String,
    public_key: String,
}

/// Get a challenge to sign for login
pub async fn get_challenge(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ChallengeRequest>,
) -> Result<Json<ChallengeResponse>> {
    // rate limit challenge requests to prevent probe attacks
    let nick_hash = user_auth::hash_nickname(&req.nickname);
    let mut redis = state.redis.clone();
    user_auth::check_rate_limit(&mut redis, "challenge", &nick_hash).await?;

    let (challenge, public_key) = user_auth::get_login_challenge(&state.db, &req.nickname).await?;

    Ok(Json(ChallengeResponse {
        challenge,
        public_key,
    }))
}

#[derive(Debug, Deserialize)]
pub struct VerifyRequest {
    nickname: String,
    challenge: String,
    signature: String,
}

/// Verify signature and create session
pub async fn verify(
    State(state): State<Arc<AppState>>,
    Json(req): Json<VerifyRequest>,
) -> Result<Json<AuthResponse>> {
    // rate limit by nickname hash to prevent brute force
    let nick_hash = user_auth::hash_nickname(&req.nickname);
    let mut redis = state.redis.clone();

    user_auth::check_rate_limit(&mut redis, "login", &nick_hash).await?;

    let result = user_auth::verify_and_login(
        &state.db,
        &req.nickname,
        &req.challenge,
        &req.signature
    ).await;

    match result {
        Ok((user, token)) => {
            // clear rate limit on success
            user_auth::clear_rate_limit(&mut redis, "login", &nick_hash).await;

            Ok(Json(AuthResponse {
                user_id: user.id.to_string(),
                nickname: user.nickname,
                email: user.email,
                balance: user.balance,
                token: Some(token),
            }))
        }
        Err(e) => Err(e),
    }
}

// ============================================================================
// Magic link auth endpoints
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct RequestMagicLinkRequest {
    email: String,
}

#[derive(Debug, Serialize)]
pub struct MagicLinkResponse {
    message: String,
    server_share: Option<String>,
}

/// Request a magic link email
pub async fn request_magic_link(
    State(state): State<Arc<AppState>>,
    Json(req): Json<RequestMagicLinkRequest>,
) -> Result<Json<MagicLinkResponse>> {
    let recovery = magic_link::request_magic_link(&state.db, &req.email).await?;

    // send email with the token
    let base_url = std::env::var("APP_URL").unwrap_or_else(|_| "https://app.sonotxt.com".into());
    magic_link::send_magic_link_email(&req.email, &recovery.token, &base_url).await?;

    // return server_share if user has shamir recovery setup
    Ok(Json(MagicLinkResponse {
        message: if recovery.server_share.is_some() {
            "Recovery share sent. Enter your saved recovery words to complete recovery.".into()
        } else {
            "Check your email for the login link".into()
        },
        server_share: recovery.server_share,
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
        nickname: None,
        email: user.email,
        balance: user.balance,
        token: Some(token),
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
    nickname: Option<String>,
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
        nickname: user.nickname,
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
