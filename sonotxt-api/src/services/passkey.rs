//! WebAuthn/passkey credential storage and Webauthn instance builder

use sqlx::PgPool;
use uuid::Uuid;
use webauthn_rs::prelude::*;

use crate::error::{ApiError, Result};

/// Build a Webauthn instance from environment or defaults
pub fn build_webauthn() -> std::result::Result<webauthn_rs::Webauthn, anyhow::Error> {
    let rp_id = std::env::var("PASSKEY_RP_ID").unwrap_or_else(|_| "sonotxt.com".to_string());
    let rp_origin =
        std::env::var("PASSKEY_RP_ORIGIN").unwrap_or_else(|_| "https://app.sonotxt.com".to_string());
    let rp_origin_url = url::Url::parse(&rp_origin)?;

    let builder = webauthn_rs::WebauthnBuilder::new(&rp_id, &rp_origin_url)?
        .rp_name("sonotxt");

    Ok(builder.build()?)
}

/// Store a passkey credential for a user.
/// The entire Passkey struct is serialized to JSON and stored in public_key.
pub async fn store_credential(
    db: &PgPool,
    user_id: Uuid,
    cred: &Passkey,
) -> Result<()> {
    let cred_id_bytes = cred.cred_id().to_vec();
    let passkey_json =
        serde_json::to_vec(cred).map_err(|e| ApiError::Internal(e.to_string()))?;

    sqlx::query(
        r#"
        INSERT INTO passkey_credentials (user_id, credential_id, public_key, counter)
        VALUES ($1, $2, $3, 0)
        "#,
    )
    .bind(user_id)
    .bind(&cred_id_bytes)
    .bind(&passkey_json)
    .execute(db)
    .await?;

    Ok(())
}

/// Look up all passkey credentials for a given user email
pub async fn get_credentials_by_email(db: &PgPool, email: &str) -> Result<Vec<Passkey>> {
    let rows: Vec<(Vec<u8>,)> = sqlx::query_as(
        r#"
        SELECT pc.public_key
        FROM passkey_credentials pc
        JOIN users u ON pc.user_id = u.id
        WHERE u.email = $1
        "#,
    )
    .bind(email)
    .fetch_all(db)
    .await?;

    let mut creds = Vec::new();
    for (pk_bytes,) in rows {
        let cred: Passkey =
            serde_json::from_slice(&pk_bytes).map_err(|e| ApiError::Internal(e.to_string()))?;
        creds.push(cred);
    }
    Ok(creds)
}

/// Update the credential after successful authentication (re-serialize entire Passkey)
pub async fn update_credential(
    db: &PgPool,
    cred: &Passkey,
) -> Result<()> {
    let cred_id_bytes = cred.cred_id().to_vec();
    let passkey_json =
        serde_json::to_vec(cred).map_err(|e| ApiError::Internal(e.to_string()))?;

    sqlx::query(
        r#"
        UPDATE passkey_credentials
        SET public_key = $1
        WHERE credential_id = $2
        "#,
    )
    .bind(&passkey_json)
    .bind(&cred_id_bytes)
    .execute(db)
    .await?;

    Ok(())
}
