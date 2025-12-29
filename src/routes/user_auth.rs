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
        // nickname + pin auth
        .route("/check-nickname", post(check_nickname))
        .route("/register", post(register))
        .route("/login", post(login))
        // magic link auth
        .route("/magic-link/request", post(request_magic_link))
        .route("/magic-link/verify", post(verify_magic_link))
        // session management
        .route("/session", post(get_session))
        .route("/logout", post(logout))
}

// ============================================================================
// Nickname + Pin auth endpoints
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
    pin: String,
}

#[derive(Debug, Serialize)]
pub struct AuthResponse {
    user_id: String,
    nickname: Option<String>,
    email: Option<String>,
    balance: f64,
    token: String,
}

/// Register with nickname + pin
pub async fn register(
    State(state): State<Arc<AppState>>,
    Json(req): Json<RegisterRequest>,
) -> Result<Json<AuthResponse>> {
    // register user
    let user = user_auth::register_with_pin(&state.db, &req.nickname, &req.pin).await?;

    // auto-login: get challenge and sign it client-side would be better,
    // but for simplicity we do it server-side here
    let challenge = user_auth::start_pin_login(&state.db, &req.nickname, &req.pin).await?;
    let signature = user_auth::sign_challenge(&req.nickname, &req.pin, &challenge);
    let (user, token) = user_auth::complete_pin_login(
        &state.db,
        &req.nickname,
        &req.pin,
        &challenge,
        &signature,
    ).await?;

    Ok(Json(AuthResponse {
        user_id: user.id.to_string(),
        nickname: Some(req.nickname),
        email: user.email,
        balance: user.balance,
        token,
    }))
}

#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    nickname: String,
    pin: String,
}

/// Login with nickname + pin
pub async fn login(
    State(state): State<Arc<AppState>>,
    Json(req): Json<LoginRequest>,
) -> Result<Json<AuthResponse>> {
    // get challenge
    let challenge = user_auth::start_pin_login(&state.db, &req.nickname, &req.pin).await?;

    // sign it (in real client-side flow, client would sign)
    let signature = user_auth::sign_challenge(&req.nickname, &req.pin, &challenge);

    // complete login
    let (user, token) = user_auth::complete_pin_login(
        &state.db,
        &req.nickname,
        &req.pin,
        &challenge,
        &signature,
    ).await?;

    Ok(Json(AuthResponse {
        user_id: user.id.to_string(),
        nickname: Some(req.nickname),
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

    // send email
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
        nickname: None,
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
