use argon2::Argon2;
use ed25519_dalek::{Signer, SigningKey, VerifyingKey, Signature, Verifier};
use sha2::{Sha256, Digest};
use sqlx::PgPool;
use uuid::Uuid;

use crate::error::{ApiError, Result};

const KEY_DERIVATION_SALT: &[u8] = b"sonotxt-ed25519-v1";

/// Combine nickname + pin into the secret used for key derivation
fn combine_credentials(nickname: &str, pin: &str) -> String {
    format!("{}:{}", nickname.to_lowercase().trim(), pin.trim())
}

/// Derive ed25519 keypair from nickname + pin
/// Uses Argon2id for key stretching, then uses output as ed25519 seed
pub fn derive_keypair(nickname: &str, pin: &str) -> (SigningKey, VerifyingKey) {
    let secret = combine_credentials(nickname, pin);
    let mut seed = [0u8; 32];

    // use argon2id with fixed params for deterministic derivation
    let argon2 = Argon2::default();
    argon2
        .hash_password_into(
            secret.as_bytes(),
            KEY_DERIVATION_SALT,
            &mut seed,
        )
        .expect("argon2 hash failed");

    let signing_key = SigningKey::from_bytes(&seed);
    let verifying_key = signing_key.verifying_key();

    (signing_key, verifying_key)
}

/// Get the public key (hex encoded) from nickname + pin
pub fn credentials_to_pubkey(nickname: &str, pin: &str) -> String {
    let (_, verifying_key) = derive_keypair(nickname, pin);
    hex::encode(verifying_key.as_bytes())
}

/// Hash just the nickname for uniqueness check (we store this to reserve the name)
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
    if !nick.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-') {
        return Err("nickname can only contain letters, numbers, _ and -");
    }
    Ok(())
}

/// Validate pin format
pub fn validate_pin(pin: &str) -> std::result::Result<(), &'static str> {
    let p = pin.trim();
    if p.len() < 4 {
        return Err("pin must be at least 4 characters");
    }
    if p.len() > 32 {
        return Err("pin must be at most 32 characters");
    }
    Ok(())
}

/// Generate a random challenge for key-based auth
pub fn generate_challenge() -> String {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).expect("getrandom failed");
    hex::encode(bytes)
}

/// Sign a challenge with the derived key
pub fn sign_challenge(nickname: &str, pin: &str, challenge: &str) -> String {
    let (signing_key, _) = derive_keypair(nickname, pin);
    let challenge_bytes = hex::decode(challenge).expect("invalid challenge hex");
    let signature = signing_key.sign(&challenge_bytes);
    hex::encode(signature.to_bytes())
}

/// Verify a signed challenge
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

    let Ok(challenge_bytes) = hex::decode(challenge) else {
        return false;
    };

    let Ok(sig_bytes) = hex::decode(signature_hex) else {
        return false;
    };
    let Ok(sig_array): std::result::Result<[u8; 64], _> = sig_bytes.try_into() else {
        return false;
    };
    let signature = Signature::from_bytes(&sig_array);

    verifying_key.verify(&challenge_bytes, &signature).is_ok()
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

#[derive(Debug, Clone)]
pub struct User {
    pub id: Uuid,
    pub email: Option<String>,
    pub public_key: Option<String>,
    pub balance: f64,
}

/// Check if a nickname is available
pub async fn is_nickname_available(db: &PgPool, nickname: &str) -> Result<bool> {
    let nick_hash = hash_nickname(nickname);

    let exists: Option<(i64,)> = sqlx::query_as(
        "SELECT 1 FROM users WHERE identifier_hash = $1"
    )
    .bind(&nick_hash)
    .fetch_optional(db)
    .await?;

    Ok(exists.is_none())
}

/// Register with nickname + pin
pub async fn register_with_pin(db: &PgPool, nickname: &str, pin: &str) -> Result<User> {
    // validate inputs
    if let Err(e) = validate_nickname(nickname) {
        return Err(ApiError::InvalidRequest(e.to_string()));
    }
    if let Err(e) = validate_pin(pin) {
        return Err(ApiError::InvalidRequest(e.to_string()));
    }

    let pubkey = credentials_to_pubkey(nickname, pin);
    let nick_hash = hash_nickname(nickname);

    // check nickname availability
    if !is_nickname_available(db, nickname).await? {
        return Err(ApiError::InvalidRequest("nickname taken".to_string()));
    }

    let user: (Uuid, Option<String>, Option<String>, f64) = sqlx::query_as(
        r#"
        INSERT INTO users (public_key, identifier_hash)
        VALUES ($1, $2)
        RETURNING id, email, public_key, balance::float8
        "#
    )
    .bind(&pubkey)
    .bind(&nick_hash)
    .fetch_one(db)
    .await?;

    Ok(User {
        id: user.0,
        email: user.1,
        public_key: user.2,
        balance: user.3,
    })
}

/// Login with nickname + pin - returns a challenge to sign
pub async fn start_pin_login(db: &PgPool, nickname: &str, pin: &str) -> Result<String> {
    let pubkey = credentials_to_pubkey(nickname, pin);

    // verify user exists
    let exists: Option<(i64,)> = sqlx::query_as(
        "SELECT 1 FROM users WHERE public_key = $1"
    )
    .bind(&pubkey)
    .fetch_optional(db)
    .await?;

    if exists.is_none() {
        return Err(ApiError::InvalidCredentials);
    }

    // create challenge
    let challenge = generate_challenge();
    let expires = chrono::Utc::now() + chrono::Duration::minutes(5);

    sqlx::query(
        r#"
        INSERT INTO auth_challenges (public_key, challenge, expires_at)
        VALUES ($1, $2, $3)
        "#
    )
    .bind(&pubkey)
    .bind(&challenge)
    .bind(expires)
    .execute(db)
    .await?;

    Ok(challenge)
}

/// Complete pin-based login with signed challenge
pub async fn complete_pin_login(
    db: &PgPool,
    nickname: &str,
    pin: &str,
    challenge: &str,
    signature: &str,
) -> Result<(User, String)> {
    let pubkey = credentials_to_pubkey(nickname, pin);

    // verify challenge exists and not expired
    let challenge_row: Option<(Uuid,)> = sqlx::query_as(
        r#"
        SELECT id FROM auth_challenges
        WHERE public_key = $1 AND challenge = $2 AND expires_at > NOW()
        "#
    )
    .bind(&pubkey)
    .bind(challenge)
    .fetch_optional(db)
    .await?;

    let Some((challenge_id,)) = challenge_row else {
        return Err(ApiError::InvalidCredentials);
    };

    // verify signature
    if !verify_signature(&pubkey, challenge, signature) {
        return Err(ApiError::InvalidCredentials);
    }

    // delete used challenge
    sqlx::query("DELETE FROM auth_challenges WHERE id = $1")
        .bind(challenge_id)
        .execute(db)
        .await?;

    // get user
    let user: (Uuid, Option<String>, Option<String>, f64) = sqlx::query_as(
        "SELECT id, email, public_key, balance::float8 FROM users WHERE public_key = $1"
    )
    .bind(&pubkey)
    .fetch_one(db)
    .await?;

    // create session
    let token = generate_session_token();
    let token_hash = hash_token(&token);
    let expires = chrono::Utc::now() + chrono::Duration::days(30);

    sqlx::query(
        r#"
        INSERT INTO sessions (user_id, token_hash, expires_at)
        VALUES ($1, $2, $3)
        "#
    )
    .bind(user.0)
    .bind(&token_hash)
    .bind(expires)
    .execute(db)
    .await?;

    // update last login
    sqlx::query("UPDATE users SET last_login = NOW() WHERE id = $1")
        .bind(user.0)
        .execute(db)
        .await?;

    Ok((
        User {
            id: user.0,
            email: user.1,
            public_key: user.2,
            balance: user.3,
        },
        token,
    ))
}

/// Validate session token and return user
pub async fn validate_session(db: &PgPool, token: &str) -> Result<User> {
    let token_hash = hash_token(token);

    let row: Option<(Uuid, Option<String>, Option<String>, f64)> = sqlx::query_as(
        r#"
        SELECT u.id, u.email, u.public_key, u.balance::float8
        FROM sessions s
        JOIN users u ON s.user_id = u.id
        WHERE s.token_hash = $1 AND s.expires_at > NOW()
        "#
    )
    .bind(&token_hash)
    .fetch_optional(db)
    .await?;

    let Some(user) = row else {
        return Err(ApiError::Unauthorized);
    };

    Ok(User {
        id: user.0,
        email: user.1,
        public_key: user.2,
        balance: user.3,
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
    fn test_key_derivation_deterministic() {
        let pubkey1 = credentials_to_pubkey("alice", "1234");
        let pubkey2 = credentials_to_pubkey("alice", "1234");
        assert_eq!(pubkey1, pubkey2);
    }

    #[test]
    fn test_different_pins_different_keys() {
        let pubkey1 = credentials_to_pubkey("alice", "1234");
        let pubkey2 = credentials_to_pubkey("alice", "5678");
        assert_ne!(pubkey1, pubkey2);
    }

    #[test]
    fn test_different_nicknames_different_keys() {
        let pubkey1 = credentials_to_pubkey("alice", "1234");
        let pubkey2 = credentials_to_pubkey("bob", "1234");
        assert_ne!(pubkey1, pubkey2);
    }

    #[test]
    fn test_sign_verify() {
        let challenge = generate_challenge();
        let signature = sign_challenge("alice", "1234", &challenge);
        let pubkey = credentials_to_pubkey("alice", "1234");

        assert!(verify_signature(&pubkey, &challenge, &signature));
    }

    #[test]
    fn test_wrong_pin_fails() {
        let challenge = generate_challenge();
        let signature = sign_challenge("alice", "1234", &challenge);
        let pubkey = credentials_to_pubkey("alice", "wrong");

        assert!(!verify_signature(&pubkey, &challenge, &signature));
    }

    #[test]
    fn test_nickname_validation() {
        assert!(validate_nickname("abc").is_ok());
        assert!(validate_nickname("alice_123").is_ok());
        assert!(validate_nickname("ab").is_err()); // too short
        assert!(validate_nickname("has space").is_err());
    }

    #[test]
    fn test_pin_validation() {
        assert!(validate_pin("1234").is_ok());
        assert!(validate_pin("mypin123").is_ok());
        assert!(validate_pin("123").is_err()); // too short
    }
}
