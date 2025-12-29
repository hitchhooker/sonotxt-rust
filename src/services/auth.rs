use chrono::{Duration, Utc};
use rand::Rng;
use sqlx::PgPool;
use uuid::Uuid;

use crate::{ApiError, Config, Result};

pub struct AuthService;

impl AuthService {
    /// generate a magic link token and send email
    pub async fn send_magic_link(db: &PgPool, config: &Config, email_addr: &str) -> Result<()> {
        let token: String = rand::thread_rng()
            .sample_iter(&rand::distributions::Alphanumeric)
            .take(64)
            .map(char::from)
            .collect();

        let expires_at = Utc::now() + Duration::minutes(15);

        sqlx::query(
            r#"
            INSERT INTO magic_links (email, token, expires_at)
            VALUES ($1, $2, $3)
            "#,
        )
        .bind(email_addr)
        .bind(&token)
        .bind(expires_at)
        .execute(db)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

        let link = format!("{}/auth/verify?token={}", config.app_url, token);

        if let (Some(jmap_url), Some(user), Some(pass)) =
            (&config.jmap_url, &config.jmap_user, &config.jmap_pass)
        {
            // send via jmap
            let html_body = format!(
                r#"<h2>Login to SonoTxt</h2>
<p>Click the link below to login. This link expires in 15 minutes.</p>
<p><a href="{link}">Login to SonoTxt</a></p>
<p>Or copy this URL: {link}</p>"#
            );

            Self::send_jmap_email(jmap_url, user, pass, &config.jmap_from, email_addr, "SonoTxt Login Link", &html_body)
                .await
                .map_err(|e| ApiError::Internal(format!("jmap send: {}", e)))?;
        } else {
            // dev mode: log the link
            tracing::info!("magic link for {}: {}", email_addr, link);
        }

        Ok(())
    }

    async fn send_jmap_email(
        jmap_url: &str,
        username: &str,
        password: &str,
        from: &str,
        to: &str,
        subject: &str,
        html_body: &str,
    ) -> std::result::Result<(), String> {
        let client = reqwest::Client::new();

        // first, get session
        let session_resp = client
            .get(jmap_url)
            .basic_auth(username, Some(password))
            .send()
            .await
            .map_err(|e| e.to_string())?;

        if !session_resp.status().is_success() {
            return Err(format!("jmap session failed: {}", session_resp.status()));
        }

        let session: serde_json::Value = session_resp.json().await.map_err(|e| e.to_string())?;

        let api_url = session
            .get("apiUrl")
            .and_then(|v| v.as_str())
            .ok_or("no apiUrl in session")?;

        let account_id = session
            .get("primaryAccounts")
            .and_then(|v| v.get("urn:ietf:params:jmap:mail"))
            .and_then(|v| v.as_str())
            .ok_or("no primary mail account")?;

        // build RFC 5322 message
        let boundary = format!("----=_{}", Uuid::new_v4());
        let message = format!(
            "From: {from}\r\n\
             To: {to}\r\n\
             Subject: {subject}\r\n\
             MIME-Version: 1.0\r\n\
             Content-Type: multipart/alternative; boundary=\"{boundary}\"\r\n\
             \r\n\
             --{boundary}\r\n\
             Content-Type: text/html; charset=utf-8\r\n\
             \r\n\
             {html_body}\r\n\
             --{boundary}--\r\n"
        );

        // upload blob
        let upload_url = session
            .get("uploadUrl")
            .and_then(|v| v.as_str())
            .ok_or("no uploadUrl")?
            .replace("{accountId}", account_id);

        let upload_resp = client
            .post(&upload_url)
            .basic_auth(username, Some(password))
            .header("Content-Type", "message/rfc822")
            .body(message)
            .send()
            .await
            .map_err(|e| e.to_string())?;

        if !upload_resp.status().is_success() {
            return Err(format!("upload failed: {}", upload_resp.status()));
        }

        let upload_result: serde_json::Value = upload_resp.json().await.map_err(|e| e.to_string())?;
        let blob_id = upload_result
            .get("blobId")
            .and_then(|v| v.as_str())
            .ok_or("no blobId")?;

        // submit email
        let submit_req = serde_json::json!({
            "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail", "urn:ietf:params:jmap:submission"],
            "methodCalls": [
                ["Email/import", {
                    "accountId": account_id,
                    "emails": {
                        "draft1": {
                            "blobId": blob_id,
                            "mailboxIds": {}
                        }
                    }
                }, "0"],
                ["EmailSubmission/set", {
                    "accountId": account_id,
                    "create": {
                        "sub1": {
                            "emailId": "#draft1",
                            "envelope": {
                                "mailFrom": { "email": from },
                                "rcptTo": [{ "email": to }]
                            }
                        }
                    }
                }, "1"]
            ]
        });

        let submit_resp = client
            .post(api_url)
            .basic_auth(username, Some(password))
            .json(&submit_req)
            .send()
            .await
            .map_err(|e| e.to_string())?;

        if !submit_resp.status().is_success() {
            return Err(format!("submit failed: {}", submit_resp.status()));
        }

        Ok(())
    }

    /// verify magic link token and create session
    pub async fn verify_magic_link(db: &PgPool, token: &str) -> Result<(Uuid, String)> {
        let row: Option<(Uuid, String, bool, chrono::DateTime<Utc>)> = sqlx::query_as(
            r#"
            SELECT id, email, used, expires_at
            FROM magic_links
            WHERE token = $1
            "#,
        )
        .bind(token)
        .fetch_optional(db)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

        let (link_id, email, used, expires_at) = row.ok_or(ApiError::Unauthorized)?;

        if used || Utc::now() > expires_at {
            return Err(ApiError::Unauthorized);
        }

        // mark as used
        sqlx::query("UPDATE magic_links SET used = true WHERE id = $1")
            .bind(link_id)
            .execute(db)
            .await
            .map_err(|e| ApiError::Internal(e.to_string()))?;

        // find or create account
        let account_id: Uuid =
            match sqlx::query_scalar::<_, Uuid>("SELECT id FROM accounts WHERE email = $1")
                .bind(&email)
                .fetch_optional(db)
                .await
                .map_err(|e| ApiError::Internal(e.to_string()))?
            {
                Some(id) => id,
                None => {
                    let id = Uuid::new_v4();
                    sqlx::query(
                        r#"
                        INSERT INTO accounts (id, email)
                        VALUES ($1, $2)
                        "#,
                    )
                    .bind(id)
                    .bind(&email)
                    .execute(db)
                    .await
                    .map_err(|e| ApiError::Internal(e.to_string()))?;

                    // create initial credits
                    sqlx::query(
                        r#"
                        INSERT INTO account_credits (account_id, balance)
                        VALUES ($1, 5.0)
                        "#,
                    )
                    .bind(id)
                    .execute(db)
                    .await
                    .map_err(|e| ApiError::Internal(e.to_string()))?;

                    id
                }
            };

        // create session
        let session_token: String = rand::thread_rng()
            .sample_iter(&rand::distributions::Alphanumeric)
            .take(64)
            .map(char::from)
            .collect();

        let expires_at = Utc::now() + Duration::days(30);

        sqlx::query(
            r#"
            INSERT INTO auth_sessions (account_id, token, expires_at)
            VALUES ($1, $2, $3)
            "#,
        )
        .bind(account_id)
        .bind(&session_token)
        .bind(expires_at)
        .execute(db)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

        Ok((account_id, session_token))
    }

    /// validate session token and return account id
    pub async fn validate_session(db: &PgPool, token: &str) -> Result<Uuid> {
        let row: Option<(Uuid, chrono::DateTime<Utc>)> = sqlx::query_as(
            r#"
            SELECT account_id, expires_at
            FROM auth_sessions
            WHERE token = $1
            "#,
        )
        .bind(token)
        .fetch_optional(db)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

        let (account_id, expires_at) = row.ok_or(ApiError::Unauthorized)?;

        if Utc::now() > expires_at {
            return Err(ApiError::Unauthorized);
        }

        Ok(account_id)
    }

    /// logout / invalidate session
    pub async fn logout(db: &PgPool, token: &str) -> Result<()> {
        sqlx::query("DELETE FROM auth_sessions WHERE token = $1")
            .bind(token)
            .execute(db)
            .await
            .map_err(|e| ApiError::Internal(e.to_string()))?;
        Ok(())
    }
}
