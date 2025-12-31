use sha2::{Sha256, Digest};
use sqlx::PgPool;
use uuid::Uuid;

use crate::error::{ApiError, Result};
use super::user_auth::{User, generate_session_token, hash_token};

/// Generate a magic link token
fn generate_magic_token() -> String {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).expect("getrandom failed");
    hex::encode(bytes)
}

/// Hash magic link token
fn hash_magic_token(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"magic-link-v1");
    hasher.update(token.as_bytes());
    hex::encode(hasher.finalize())
}

/// Request a magic link for email
/// Returns the token (to be sent via email)
pub async fn request_magic_link(db: &PgPool, email: &str) -> Result<String> {
    let email = email.to_lowercase().trim().to_string();

    // basic email validation
    if !email.contains('@') || email.len() < 5 {
        return Err(ApiError::InvalidRequestError);
    }

    // rate limit: max 3 magic links per email per hour
    let recent_count: (i64,) = sqlx::query_as(
        r#"
        SELECT COUNT(*) FROM magic_links
        WHERE email = $1 AND created_at > NOW() - INTERVAL '1 hour'
        "#
    )
    .bind(&email)
    .fetch_one(db)
    .await?;

    if recent_count.0 >= 3 {
        return Err(ApiError::RateLimited);
    }

    let token = generate_magic_token();
    let token_hash = hash_magic_token(&token);
    let expires = chrono::Utc::now() + chrono::Duration::minutes(15);

    sqlx::query(
        r#"
        INSERT INTO magic_links (email, token_hash, expires_at)
        VALUES ($1, $2, $3)
        "#
    )
    .bind(&email)
    .bind(&token_hash)
    .bind(expires)
    .execute(db)
    .await?;

    Ok(token)
}

/// Verify magic link and create session
/// Creates user if doesn't exist
pub async fn verify_magic_link(db: &PgPool, token: &str) -> Result<(User, String)> {
    let token_hash = hash_magic_token(token);

    // find valid magic link
    let link: Option<(Uuid, String)> = sqlx::query_as(
        r#"
        SELECT id, email FROM magic_links
        WHERE token_hash = $1 AND expires_at > NOW() AND used = FALSE
        "#
    )
    .bind(&token_hash)
    .fetch_optional(db)
    .await?;

    let Some((link_id, email)) = link else {
        return Err(ApiError::InvalidCredentials);
    };

    // mark as used
    sqlx::query("UPDATE magic_links SET used = TRUE WHERE id = $1")
        .bind(link_id)
        .execute(db)
        .await?;

    // find or create user
    let user: (Uuid, Option<String>, Option<String>, Option<String>, f64) = sqlx::query_as(
        r#"
        INSERT INTO users (email, email_verified)
        VALUES ($1, TRUE)
        ON CONFLICT (email) DO UPDATE SET email_verified = TRUE, last_login = NOW()
        RETURNING id, nickname, email, public_key, balance::float8
        "#
    )
    .bind(&email)
    .fetch_one(db)
    .await?;

    // create session
    let session_token = generate_session_token();
    let session_hash = hash_token(&session_token);
    let expires = chrono::Utc::now() + chrono::Duration::days(30);

    sqlx::query(
        r#"
        INSERT INTO sessions (user_id, token_hash, expires_at)
        VALUES ($1, $2, $3)
        "#
    )
    .bind(user.0)
    .bind(&session_hash)
    .bind(expires)
    .execute(db)
    .await?;

    Ok((
        User {
            id: user.0,
            nickname: user.1,
            email: user.2,
            public_key: user.3,
            balance: user.4,
        },
        session_token,
    ))
}

/// Send magic link email (stub - implement with your email provider)
pub async fn send_magic_link_email(email: &str, token: &str, base_url: &str) -> Result<()> {
    let link = format!("{}/auth/verify?token={}", base_url, token);

    // TODO: integrate with email service (sendgrid, ses, resend, etc)
    // For now, log it
    tracing::info!("Magic link for {}: {}", email, link);

    Ok(())
}
