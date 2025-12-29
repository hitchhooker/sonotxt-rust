use axum::{
    extract::{Query, State},
    http::{header, StatusCode},
    routing::{get, post},
    Json, Router,
};
use axum_extra::TypedHeader;
use headers::{authorization::Bearer, Authorization};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::{services::auth::AuthService, ApiError, AppState, Result};

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/auth/login", post(login))
        .route("/auth/verify", get(verify))
        .route("/auth/logout", post(logout))
        .route("/auth/me", get(me))
}

#[derive(Deserialize)]
pub struct LoginRequest {
    email: String,
}

#[derive(Serialize)]
pub struct LoginResponse {
    message: String,
}

async fn login(
    State(state): State<Arc<AppState>>,
    Json(req): Json<LoginRequest>,
) -> Result<Json<LoginResponse>> {
    AuthService::send_magic_link(&state.db, &state.config, &req.email).await?;

    Ok(Json(LoginResponse {
        message: "check your email for login link".into(),
    }))
}

#[derive(Deserialize)]
pub struct VerifyQuery {
    token: String,
}

#[derive(Serialize)]
pub struct VerifyResponse {
    session_token: String,
    account_id: String,
}

async fn verify(
    State(state): State<Arc<AppState>>,
    Query(query): Query<VerifyQuery>,
) -> Result<Json<VerifyResponse>> {
    let (account_id, session_token) =
        AuthService::verify_magic_link(&state.db, &query.token).await?;

    Ok(Json(VerifyResponse {
        session_token,
        account_id: account_id.to_string(),
    }))
}

#[derive(Deserialize)]
pub struct LogoutRequest {
    session_token: String,
}

async fn logout(
    State(state): State<Arc<AppState>>,
    Json(req): Json<LogoutRequest>,
) -> Result<StatusCode> {
    AuthService::logout(&state.db, &req.session_token).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize)]
pub struct MeResponse {
    account_id: String,
    email: Option<String>,
    balance: f64,
}

async fn me(
    State(state): State<Arc<AppState>>,
    TypedHeader(auth): TypedHeader<Authorization<Bearer>>,
) -> Result<Json<MeResponse>> {
    let token = auth.token();
    let account_id = AuthService::validate_session(&state.db, token).await?;

    let row: Option<(Option<String>, f64)> = sqlx::query_as(
        r#"
        SELECT a.email, COALESCE(c.balance, 0.0)
        FROM accounts a
        LEFT JOIN account_credits c ON c.account_id = a.id
        WHERE a.id = $1
        "#,
    )
    .bind(account_id)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| crate::ApiError::Internal(e.to_string()))?;

    let (email, balance) = row.unwrap_or((None, 0.0));

    Ok(Json(MeResponse {
        account_id: account_id.to_string(),
        email,
        balance,
    }))
}
