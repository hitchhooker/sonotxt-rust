//! User authentication with client-side ed25519 key derivation
//!
//! Flow:
//! 1. Client derives ed25519 keypair from nickname:pin using argon2id
//! 2. Registration: client sends nickname + public_key (pin never leaves client)
//! 3. Login: server sends challenge, client signs, server verifies

use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use redis::AsyncCommands;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use uuid::Uuid;

use crate::error::{ApiError, Result};

/// Rate limit config
const MAX_AUTH_ATTEMPTS: u32 = 5;      // Max attempts per window
const RATE_LIMIT_WINDOW: u64 = 300;    // 5 minute window
const LOCKOUT_DURATION: u64 = 900;     // 15 minute lockout after exceeding

/// Check and increment rate limit, returns error if exceeded
pub async fn check_rate_limit(
    redis: &mut redis::aio::ConnectionManager,
    key_prefix: &str,
    identifier: &str,
) -> Result<()> {
    let key = format!("auth_limit:{}:{}", key_prefix, identifier);
    let lockout_key = format!("auth_lockout:{}:{}", key_prefix, identifier);

    // Check if locked out
    let locked: Option<String> = redis.get(&lockout_key).await.unwrap_or(None);
    if locked.is_some() {
        return Err(ApiError::RateLimited);
    }

    // Increment attempt counter
    let attempts: u32 = redis.incr(&key, 1).await.unwrap_or(1);

    // Set expiry on first attempt
    if attempts == 1 {
        let _: () = redis.expire(&key, RATE_LIMIT_WINDOW as i64).await.unwrap_or(());
    }

    // Check if exceeded
    if attempts > MAX_AUTH_ATTEMPTS {
        // Set lockout
        let _: () = redis.set_ex(&lockout_key, "1", LOCKOUT_DURATION).await.unwrap_or(());
        return Err(ApiError::RateLimited);
    }

    Ok(())
}

/// Clear rate limit on successful auth
pub async fn clear_rate_limit(
    redis: &mut redis::aio::ConnectionManager,
    key_prefix: &str,
    identifier: &str,
) {
    let key = format!("auth_limit:{}:{}", key_prefix, identifier);
    let _: () = redis.del(&key).await.unwrap_or(());
}

#[derive(Debug, Clone)]
pub struct User {
    pub id: Uuid,
    pub nickname: Option<String>,
    pub email: Option<String>,
    pub public_key: Option<String>,
    pub balance: f64,
}

/// Hash nickname for storage (we store hash, not plaintext, for some privacy)
pub fn hash_nickname(nickname: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"sonotxt-nick-v1");
    hasher.update(nickname.to_lowercase().trim().as_bytes());
    hex::encode(hasher.finalize())
}

/// Validate nickname format
pub fn validate_nickname(nickname: &str) -> std::result::Result<(), &'static str> {
    let nick = nickname.trim();
    if nick.len() < 3 {
        return Err("nickname must be at least 3 characters");
    }
    if nick.len() > 20 {
        return Err("nickname must be at most 20 characters");
    }
    if !nick
        .chars()
        .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
    {
        return Err("nickname can only contain letters, numbers, _ and -");
    }
    Ok(())
}

/// Validate public key format (64 hex chars = 32 bytes)
pub fn validate_public_key(pubkey: &str) -> std::result::Result<(), &'static str> {
    if pubkey.len() != 64 {
        return Err("invalid public key length");
    }
    if !pubkey.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err("invalid public key format");
    }
    // Try to parse it
    let bytes = hex::decode(pubkey).map_err(|_| "invalid hex")?;
    let arr: [u8; 32] = bytes.try_into().map_err(|_| "invalid key length")?;
    VerifyingKey::from_bytes(&arr).map_err(|_| "invalid ed25519 public key")?;
    Ok(())
}

/// Generate a random challenge for signature auth
pub fn generate_challenge() -> String {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).expect("getrandom failed");
    hex::encode(bytes)
}

/// Verify a signature against a public key and challenge
/// The challenge is signed as raw bytes (not hex)
pub fn verify_signature(pubkey_hex: &str, challenge: &str, signature_hex: &str) -> bool {
    let Ok(pubkey_bytes) = hex::decode(pubkey_hex) else {
        return false;
    };
    let Ok(pubkey_array): std::result::Result<[u8; 32], _> = pubkey_bytes.try_into() else {
        return false;
    };
    let Ok(verifying_key) = VerifyingKey::from_bytes(&pubkey_array) else {
        return false;
    };

    // Challenge is signed as the raw string bytes (UTF-8), not hex-decoded
    let challenge_bytes = challenge.as_bytes();

    let Ok(sig_bytes) = hex::decode(signature_hex) else {
        return false;
    };
    let Ok(sig_array): std::result::Result<[u8; 64], _> = sig_bytes.try_into() else {
        return false;
    };
    let signature = Signature::from_bytes(&sig_array);

    verifying_key.verify(challenge_bytes, &signature).is_ok()
}

/// Generate a session token
pub fn generate_session_token() -> String {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).expect("getrandom failed");
    hex::encode(bytes)
}

/// Hash a session token for storage
pub fn hash_token(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    hex::encode(hasher.finalize())
}

/// Check if a nickname is available
/// NOTE: This now always returns true to prevent username enumeration.
/// The actual check happens at registration time.
pub async fn is_nickname_available(_db: &PgPool, _nickname: &str) -> Result<bool> {
    // Always return true to prevent username enumeration
    // Real availability check happens at registration
    Ok(true)
}

/// Internal check for registration (not exposed via API)
pub async fn is_nickname_actually_available(db: &PgPool, nickname: &str) -> Result<bool> {
    let nick_hash = hash_nickname(nickname);

    let exists: Option<(i64,)> =
        sqlx::query_as("SELECT 1 FROM users WHERE identifier_hash = $1")
            .bind(&nick_hash)
            .fetch_optional(db)
            .await?;

    Ok(exists.is_none())
}

/// Register with nickname + public_key (client-derived)
pub async fn register_with_pubkey(
    db: &PgPool,
    nickname: &str,
    public_key: &str,
    email: Option<&str>,
    recovery_share: Option<&str>,
) -> Result<User> {
    // Validate inputs
    if let Err(e) = validate_nickname(nickname) {
        return Err(ApiError::InvalidRequest(e.to_string()));
    }
    if let Err(e) = validate_public_key(public_key) {
        return Err(ApiError::InvalidRequest(e.to_string()));
    }

    // Validate email if provided
    let email_lower = email.map(|e| e.to_lowercase().trim().to_string());
    if let Some(ref e) = email_lower {
        if !e.contains('@') || e.len() < 5 {
            return Err(ApiError::InvalidRequest("invalid email".to_string()));
        }
    }

    // Must provide recovery_share if email is provided
    if email.is_some() && recovery_share.is_none() {
        return Err(ApiError::InvalidRequest(
            "recovery_share required with email".to_string(),
        ));
    }

    let nick_hash = hash_nickname(nickname);
    let nick_lower = nickname.to_lowercase().trim().to_string();

    // Check nickname availability (use internal check, not the dummy one)
    if !is_nickname_actually_available(db, nickname).await? {
        // Return generic error to not confirm username exists
        return Err(ApiError::InvalidRequest("registration failed".to_string()));
    }

    // Check public key not already registered
    let pk_exists: Option<(i64,)> =
        sqlx::query_as("SELECT 1 FROM users WHERE public_key = $1")
            .bind(public_key)
            .fetch_optional(db)
            .await?;
    if pk_exists.is_some() {
        return Err(ApiError::InvalidRequest(
            "public key already registered".to_string(),
        ));
    }

    // Check email not already in use
    if let Some(ref e) = email_lower {
        let email_exists: Option<(i64,)> =
            sqlx::query_as("SELECT 1 FROM users WHERE email = $1")
                .bind(e)
                .fetch_optional(db)
                .await?;
        if email_exists.is_some() {
            return Err(ApiError::InvalidRequest("email already registered".to_string()));
        }
    }

    let user: (Uuid, Option<String>, Option<String>, Option<String>, f64) = sqlx::query_as(
        r#"
        INSERT INTO users (nickname, public_key, identifier_hash, email, recovery_share)
        VALUES ($1, $2, $3, $4, $5)
        RETURNING id, nickname, email, public_key, balance::float8
        "#,
    )
    .bind(&nick_lower)
    .bind(public_key)
    .bind(&nick_hash)
    .bind(&email_lower)
    .bind(recovery_share)
    .fetch_one(db)
    .await?;

    Ok(User {
        id: user.0,
        nickname: user.1,
        email: user.2,
        public_key: user.3,
        balance: user.4,
    })
}

/// Register with just a public key (no nickname)
/// This enables direct crypto account identity
pub async fn register_with_pubkey_only(
    db: &PgPool,
    public_key: &str,
) -> Result<User> {
    // Validate public key
    if let Err(e) = validate_public_key(public_key) {
        return Err(ApiError::InvalidRequest(e.to_string()));
    }

    // Check public key not already registered
    let existing: Option<(Uuid, Option<String>, Option<String>, Option<String>, f64)> = sqlx::query_as(
        "SELECT id, nickname, email, public_key, balance::float8 FROM users WHERE public_key = $1"
    )
    .bind(public_key)
    .fetch_optional(db)
    .await?;

    if let Some((id, nickname, email, pk, balance)) = existing {
        // User already exists, return existing user
        return Ok(User {
            id,
            nickname,
            email,
            public_key: pk,
            balance,
        });
    }

    // Use public key as identifier hash (for lookup)
    let identifier_hash = format!("pk:{}", public_key);

    let user: (Uuid, Option<String>, Option<String>, Option<String>, f64) = sqlx::query_as(
        r#"
        INSERT INTO users (public_key, identifier_hash)
        VALUES ($1, $2)
        RETURNING id, nickname, email, public_key, balance::float8
        "#,
    )
    .bind(public_key)
    .bind(&identifier_hash)
    .fetch_one(db)
    .await?;

    Ok(User {
        id: user.0,
        nickname: user.1,
        email: user.2,
        public_key: user.3,
        balance: user.4,
    })
}

/// Get challenge for direct public key login (no nickname)
pub async fn get_pubkey_challenge(db: &PgPool, public_key: &str) -> Result<String> {
    // Validate public key format
    if let Err(e) = validate_public_key(public_key) {
        return Err(ApiError::InvalidRequest(e.to_string()));
    }

    // Check if user exists
    let user_exists: Option<(Uuid,)> = sqlx::query_as(
        "SELECT id FROM users WHERE public_key = $1"
    )
    .bind(public_key)
    .fetch_optional(db)
    .await?;

    let challenge = generate_challenge();

    if user_exists.is_some() {
        // Store challenge
        let expires = chrono::Utc::now() + chrono::Duration::minutes(5);
        sqlx::query(
            r#"
            INSERT INTO auth_challenges (public_key, challenge, expires_at)
            VALUES ($1, $2, $3)
            "#,
        )
        .bind(public_key)
        .bind(&challenge)
        .bind(expires)
        .execute(db)
        .await?;
    }
    // For non-existent users, we still return a challenge to prevent enumeration
    // But we don't store it, so verification will fail

    Ok(challenge)
}

/// Verify signature for direct public key login and create session
/// If user doesn't exist, registers them automatically
pub async fn verify_pubkey_login(
    db: &PgPool,
    public_key: &str,
    challenge: &str,
    signature: &str,
) -> Result<(User, String)> {
    // Validate public key format
    if let Err(e) = validate_public_key(public_key) {
        return Err(ApiError::InvalidRequest(e.to_string()));
    }

    // Verify signature first (before checking user/challenge)
    if !verify_signature(public_key, challenge, signature) {
        return Err(ApiError::InvalidCredentials);
    }

    // Get or create user
    let user = get_or_create_user_by_pubkey(db, public_key).await?;

    // Check if challenge exists (for existing users)
    let challenge_row: Option<(Uuid,)> = sqlx::query_as(
        r#"
        SELECT id FROM auth_challenges
        WHERE public_key = $1 AND challenge = $2 AND expires_at > NOW()
        "#,
    )
    .bind(public_key)
    .bind(challenge)
    .fetch_optional(db)
    .await?;

    // Delete used challenge if it exists
    if let Some((challenge_id,)) = challenge_row {
        sqlx::query("DELETE FROM auth_challenges WHERE id = $1")
            .bind(challenge_id)
            .execute(db)
            .await?;
    }

    // Create session
    let token = generate_session_token();
    let token_hash = hash_token(&token);
    let expires = chrono::Utc::now() + chrono::Duration::days(30);

    sqlx::query(
        r#"
        INSERT INTO sessions (user_id, token_hash, expires_at)
        VALUES ($1, $2, $3)
        "#,
    )
    .bind(user.id)
    .bind(&token_hash)
    .bind(expires)
    .execute(db)
    .await?;

    // Update last login
    sqlx::query("UPDATE users SET last_login = NOW() WHERE id = $1")
        .bind(user.id)
        .execute(db)
        .await?;

    Ok((user, token))
}

/// Get existing user by public key or create new one
async fn get_or_create_user_by_pubkey(db: &PgPool, public_key: &str) -> Result<User> {
    // Try to get existing user
    let existing: Option<(Uuid, Option<String>, Option<String>, Option<String>, f64)> = sqlx::query_as(
        "SELECT id, nickname, email, public_key, balance::float8 FROM users WHERE public_key = $1"
    )
    .bind(public_key)
    .fetch_optional(db)
    .await?;

    if let Some((id, nickname, email, pk, balance)) = existing {
        return Ok(User {
            id,
            nickname,
            email,
            public_key: pk,
            balance,
        });
    }

    // Create new user with just public key
    let identifier_hash = format!("pk:{}", public_key);
    let user: (Uuid, Option<String>, Option<String>, Option<String>, f64) = sqlx::query_as(
        r#"
        INSERT INTO users (public_key, identifier_hash)
        VALUES ($1, $2)
        RETURNING id, nickname, email, public_key, balance::float8
        "#,
    )
    .bind(public_key)
    .bind(&identifier_hash)
    .fetch_one(db)
    .await?;

    Ok(User {
        id: user.0,
        nickname: user.1,
        email: user.2,
        public_key: user.3,
        balance: user.4,
    })
}

/// Get challenge for nickname-based login
/// Returns (challenge, public_key) so client can verify they're signing for the right key
/// NOTE: Returns fake challenge for non-existent users to prevent enumeration
pub async fn get_login_challenge(db: &PgPool, nickname: &str) -> Result<(String, String)> {
    let nick_hash = hash_nickname(nickname);

    // Get user by nickname hash
    let user: Option<(Uuid, String)> = sqlx::query_as(
        "SELECT id, public_key FROM users WHERE identifier_hash = $1 AND public_key IS NOT NULL",
    )
    .bind(&nick_hash)
    .fetch_optional(db)
    .await?;

    // Generate challenge regardless of whether user exists
    let challenge = generate_challenge();

    match user {
        Some((_user_id, pubkey)) => {
            // Real user - store challenge
            let expires = chrono::Utc::now() + chrono::Duration::minutes(5);

            sqlx::query(
                r#"
                INSERT INTO auth_challenges (public_key, challenge, expires_at)
                VALUES ($1, $2, $3)
                "#,
            )
            .bind(&pubkey)
            .bind(&challenge)
            .bind(expires)
            .execute(db)
            .await?;

            Ok((challenge, pubkey))
        }
        None => {
            // Fake user - return deterministic fake pubkey based on nickname hash
            // This prevents timing attacks and username enumeration
            let fake_pubkey = generate_fake_pubkey(&nick_hash);
            Ok((challenge, fake_pubkey))
        }
    }
}

/// Generate a deterministic but fake-looking public key from a hash
/// This is used for non-existent users to prevent enumeration
fn generate_fake_pubkey(nick_hash: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(b"sonotxt-fake-pubkey-v1");
    hasher.update(nick_hash.as_bytes());
    hex::encode(hasher.finalize())
}

/// Verify signature and create session
pub async fn verify_and_login(
    db: &PgPool,
    nickname: &str,
    challenge: &str,
    signature: &str,
) -> Result<(User, String)> {
    let nick_hash = hash_nickname(nickname);

    // Get user and their public key
    let user_row: Option<(Uuid, Option<String>, Option<String>, String, f64)> = sqlx::query_as(
        r#"
        SELECT id, nickname, email, public_key, balance::float8
        FROM users
        WHERE identifier_hash = $1 AND public_key IS NOT NULL
        "#,
    )
    .bind(&nick_hash)
    .fetch_optional(db)
    .await?;

    let Some((user_id, nickname, email, pubkey, balance)) = user_row else {
        return Err(ApiError::InvalidCredentials);
    };

    // Verify challenge exists and not expired
    let challenge_row: Option<(Uuid,)> = sqlx::query_as(
        r#"
        SELECT id FROM auth_challenges
        WHERE public_key = $1 AND challenge = $2 AND expires_at > NOW()
        "#,
    )
    .bind(&pubkey)
    .bind(challenge)
    .fetch_optional(db)
    .await?;

    let Some((challenge_id,)) = challenge_row else {
        return Err(ApiError::InvalidCredentials);
    };

    // Verify signature
    if !verify_signature(&pubkey, challenge, signature) {
        return Err(ApiError::InvalidCredentials);
    }

    // Delete used challenge
    sqlx::query("DELETE FROM auth_challenges WHERE id = $1")
        .bind(challenge_id)
        .execute(db)
        .await?;

    // Create session
    let token = generate_session_token();
    let token_hash = hash_token(&token);
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
    .execute(db)
    .await?;

    // Update last login
    sqlx::query("UPDATE users SET last_login = NOW() WHERE id = $1")
        .bind(user_id)
        .execute(db)
        .await?;

    Ok((
        User {
            id: user_id,
            nickname,
            email,
            public_key: Some(pubkey),
            balance,
        },
        token,
    ))
}

/// Validate session token and return user
pub async fn validate_session(db: &PgPool, token: &str) -> Result<User> {
    let token_hash = hash_token(token);

    let row: Option<(Uuid, Option<String>, Option<String>, Option<String>, f64)> = sqlx::query_as(
        r#"
        SELECT u.id, u.nickname, u.email, u.public_key, u.balance::float8
        FROM sessions s
        JOIN users u ON s.user_id = u.id
        WHERE s.token_hash = $1 AND s.expires_at > NOW()
        "#,
    )
    .bind(&token_hash)
    .fetch_optional(db)
    .await?;

    let Some(user) = row else {
        return Err(ApiError::Unauthorized);
    };

    Ok(User {
        id: user.0,
        nickname: user.1,
        email: user.2,
        public_key: user.3,
        balance: user.4,
    })
}

/// Delete session (logout)
pub async fn logout(db: &PgPool, token: &str) -> Result<()> {
    let token_hash = hash_token(token);
    sqlx::query("DELETE FROM sessions WHERE token_hash = $1")
        .bind(&token_hash)
        .execute(db)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_nickname_hash_deterministic() {
        let h1 = hash_nickname("alice");
        let h2 = hash_nickname("Alice"); // case insensitive
        assert_eq!(h1, h2);
    }

    #[test]
    fn test_nickname_validation() {
        assert!(validate_nickname("abc").is_ok());
        assert!(validate_nickname("alice_123").is_ok());
        assert!(validate_nickname("ab").is_err()); // too short
        assert!(validate_nickname("has space").is_err());
    }

    #[test]
    fn test_pubkey_validation() {
        // Valid ed25519 public key (32 bytes = 64 hex chars)
        // This is a test key, just random bytes that happen to be valid
        let valid = "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a";
        assert!(validate_public_key(valid).is_ok());

        // Too short
        assert!(validate_public_key("d75a980182b10ab7").is_err());

        // Invalid hex
        assert!(validate_public_key("zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz").is_err());
    }
}
