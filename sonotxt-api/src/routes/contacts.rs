use axum::{
    extract::State,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use uuid::Uuid;

use crate::{
    auth::api_key::AuthenticatedUser,
    error::{ApiError, Result},
    AppState,
};

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/", get(list_contacts))
        .route("/pending", get(list_pending))
        .route("/invite", post(send_invite))
        .route("/accept", post(accept_invite))
        .route("/reject", post(reject_invite))
        .route("/block", post(block_contact))
        .route("/remove", post(remove_contact))
        .route("/lookup", post(lookup_user))
}

// --- Request/Response types ---

#[derive(Deserialize)]
struct InviteRequest {
    /// Invite by wallet address (SS58), nickname, or email
    address: Option<String>,
    nickname: Option<String>,
    email: Option<String>,
    /// Optional message with the invite
    message: Option<String>,
}

#[derive(Deserialize)]
struct ContactAction {
    contact_id: Uuid,
}

#[derive(Deserialize)]
struct LookupRequest {
    /// SS58 wallet address to look up
    address: Option<String>,
    /// Nickname to look up
    nickname: Option<String>,
}

#[derive(Serialize)]
struct Contact {
    id: Uuid,
    user_id: Uuid,
    contact_id: Uuid,
    status: String,
    message: Option<String>,
    created_at: String,
    accepted_at: Option<String>,
    // Contact's profile info
    nickname: Option<String>,
    wallet_address: Option<String>,
    email: Option<String>,
}

#[derive(Serialize)]
struct UserLookup {
    id: Uuid,
    nickname: Option<String>,
    wallet_address: Option<String>,
    /// On-chain identity display name from PeopleChain (if available)
    identity_display: Option<String>,
    /// Whether this user is already a contact
    is_contact: bool,
    contact_status: Option<String>,
}

// --- Handlers ---

/// List accepted contacts
async fn list_contacts(
    user: AuthenticatedUser,
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<Contact>>> {
    let rows = sqlx::query_as::<_, (Uuid, Uuid, Uuid, String, Option<String>, chrono::DateTime<chrono::Utc>, Option<chrono::DateTime<chrono::Utc>>, Option<String>, Option<String>, Option<String>)>(
        r#"
        SELECT c.id, c.user_id, c.contact_id, c.status, c.message, c.created_at, c.accepted_at,
               u.nickname, u.wallet_address, u.email
        FROM contacts c
        JOIN users u ON u.id = CASE
            WHEN c.user_id = $1 THEN c.contact_id
            ELSE c.user_id
        END
        WHERE (c.user_id = $1 OR c.contact_id = $1)
          AND c.status = 'accepted'
        ORDER BY c.accepted_at DESC
        "#
    )
    .bind(user.account_id)
    .fetch_all(&state.db)
    .await
    .map_err(|_| ApiError::InternalError)?;

    let contacts: Vec<Contact> = rows
        .into_iter()
        .map(|(id, user_id, contact_id, status, message, created_at, accepted_at, nickname, wallet_address, email)| Contact {
            id,
            user_id,
            contact_id,
            status,
            message,
            created_at: created_at.to_rfc3339(),
            accepted_at: accepted_at.map(|t| t.to_rfc3339()),
            nickname,
            wallet_address,
            email: email.map(|e| {
                // Mask email: show first 2 chars + domain
                if let Some(at) = e.find('@') {
                    let prefix = &e[..at.min(2)];
                    format!("{}...{}", prefix, &e[at..])
                } else {
                    "***".to_string()
                }
            }),
        })
        .collect();

    Ok(Json(contacts))
}

/// List pending invites (received)
async fn list_pending(
    user: AuthenticatedUser,
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<Contact>>> {
    let rows = sqlx::query_as::<_, (Uuid, Uuid, Uuid, String, Option<String>, chrono::DateTime<chrono::Utc>, Option<String>, Option<String>, Option<String>)>(
        r#"
        SELECT c.id, c.user_id, c.contact_id, c.status, c.message, c.created_at,
               u.nickname, u.wallet_address, u.email
        FROM contacts c
        JOIN users u ON u.id = c.user_id
        WHERE c.contact_id = $1 AND c.status = 'pending'
        ORDER BY c.created_at DESC
        "#
    )
    .bind(user.account_id)
    .fetch_all(&state.db)
    .await
    .map_err(|_| ApiError::InternalError)?;

    let contacts: Vec<Contact> = rows
        .into_iter()
        .map(|(id, user_id, contact_id, status, message, created_at, nickname, wallet_address, email)| Contact {
            id,
            user_id,
            contact_id,
            status,
            message,
            created_at: created_at.to_rfc3339(),
            accepted_at: None,
            nickname,
            wallet_address,
            email: email.map(|e| {
                if let Some(at) = e.find('@') {
                    let prefix = &e[..at.min(2)];
                    format!("{}...{}", prefix, &e[at..])
                } else {
                    "***".to_string()
                }
            }),
        })
        .collect();

    Ok(Json(contacts))
}

/// Send a contact invitation
async fn send_invite(
    user: AuthenticatedUser,
    State(state): State<Arc<AppState>>,
    Json(req): Json<InviteRequest>,
) -> Result<Json<serde_json::Value>> {
    // Find the target user
    let target_id: Option<Uuid> = if let Some(ref addr) = req.address {
        sqlx::query_scalar("SELECT id FROM users WHERE wallet_address = $1")
            .bind(addr)
            .fetch_optional(&state.db)
            .await
            .map_err(|_| ApiError::InternalError)?
    } else if let Some(ref nick) = req.nickname {
        sqlx::query_scalar("SELECT id FROM users WHERE LOWER(nickname) = LOWER($1)")
            .bind(nick)
            .fetch_optional(&state.db)
            .await
            .map_err(|_| ApiError::InternalError)?
    } else if let Some(ref email) = req.email {
        sqlx::query_scalar("SELECT id FROM users WHERE LOWER(email) = LOWER($1)")
            .bind(email)
            .fetch_optional(&state.db)
            .await
            .map_err(|_| ApiError::InternalError)?
    } else {
        return Err(ApiError::InvalidRequest("provide address, nickname, or email".into()));
    };

    let target_id = target_id
        .ok_or_else(|| ApiError::InvalidRequest("user not found".into()))?;

    if target_id == user.account_id {
        return Err(ApiError::InvalidRequest("cannot invite yourself".into()));
    }

    // Check if invite already exists (in either direction)
    let existing: Option<String> = sqlx::query_scalar(
        r#"
        SELECT status FROM contacts
        WHERE (user_id = $1 AND contact_id = $2)
           OR (user_id = $2 AND contact_id = $1)
        "#
    )
    .bind(user.account_id)
    .bind(target_id)
    .fetch_optional(&state.db)
    .await
    .map_err(|_| ApiError::InternalError)?;

    if let Some(status) = existing {
        return Ok(Json(serde_json::json!({
            "status": "existing",
            "current": status,
        })));
    }

    // Create the invite
    sqlx::query(
        r#"
        INSERT INTO contacts (user_id, contact_id, status, message)
        VALUES ($1, $2, 'pending', $3)
        "#
    )
    .bind(user.account_id)
    .bind(target_id)
    .bind(&req.message)
    .execute(&state.db)
    .await
    .map_err(|_| ApiError::InternalError)?;

    Ok(Json(serde_json::json!({ "status": "invited" })))
}

/// Accept a pending invite
async fn accept_invite(
    user: AuthenticatedUser,
    State(state): State<Arc<AppState>>,
    Json(req): Json<ContactAction>,
) -> Result<Json<serde_json::Value>> {
    let result = sqlx::query(
        r#"
        UPDATE contacts
        SET status = 'accepted', accepted_at = now()
        WHERE id = $1 AND contact_id = $2 AND status = 'pending'
        "#
    )
    .bind(req.contact_id)
    .bind(user.account_id)
    .execute(&state.db)
    .await
    .map_err(|_| ApiError::InternalError)?;

    if result.rows_affected() == 0 {
        return Err(ApiError::InvalidRequest("invite not found or already handled".into()));
    }

    Ok(Json(serde_json::json!({ "status": "accepted" })))
}

/// Reject a pending invite
async fn reject_invite(
    user: AuthenticatedUser,
    State(state): State<Arc<AppState>>,
    Json(req): Json<ContactAction>,
) -> Result<Json<serde_json::Value>> {
    let result = sqlx::query(
        "DELETE FROM contacts WHERE id = $1 AND contact_id = $2 AND status = 'pending'"
    )
    .bind(req.contact_id)
    .bind(user.account_id)
    .execute(&state.db)
    .await
    .map_err(|_| ApiError::InternalError)?;

    if result.rows_affected() == 0 {
        return Err(ApiError::InvalidRequest("invite not found".into()));
    }

    Ok(Json(serde_json::json!({ "status": "rejected" })))
}

/// Block a contact (or pending invite)
async fn block_contact(
    user: AuthenticatedUser,
    State(state): State<Arc<AppState>>,
    Json(req): Json<ContactAction>,
) -> Result<Json<serde_json::Value>> {
    let result = sqlx::query(
        r#"
        UPDATE contacts
        SET status = 'blocked'
        WHERE id = $1 AND (contact_id = $2 OR user_id = $2)
        "#
    )
    .bind(req.contact_id)
    .bind(user.account_id)
    .execute(&state.db)
    .await
    .map_err(|_| ApiError::InternalError)?;

    if result.rows_affected() == 0 {
        return Err(ApiError::InvalidRequest("contact not found".into()));
    }

    Ok(Json(serde_json::json!({ "status": "blocked" })))
}

/// Remove an accepted contact
async fn remove_contact(
    user: AuthenticatedUser,
    State(state): State<Arc<AppState>>,
    Json(req): Json<ContactAction>,
) -> Result<Json<serde_json::Value>> {
    let result = sqlx::query(
        r#"
        DELETE FROM contacts
        WHERE id = $1 AND (user_id = $2 OR contact_id = $2)
        "#
    )
    .bind(req.contact_id)
    .bind(user.account_id)
    .execute(&state.db)
    .await
    .map_err(|_| ApiError::InternalError)?;

    if result.rows_affected() == 0 {
        return Err(ApiError::InvalidRequest("contact not found".into()));
    }

    Ok(Json(serde_json::json!({ "status": "removed" })))
}

/// Look up a user by wallet address or nickname
/// Includes PeopleChain identity display name if available
async fn lookup_user(
    user: AuthenticatedUser,
    State(state): State<Arc<AppState>>,
    Json(req): Json<LookupRequest>,
) -> Result<Json<Vec<UserLookup>>> {
    let rows = if let Some(ref addr) = req.address {
        sqlx::query_as::<_, (Uuid, Option<String>, Option<String>)>(
            "SELECT id, nickname, wallet_address FROM users WHERE wallet_address = $1"
        )
        .bind(addr)
        .fetch_all(&state.db)
        .await
        .map_err(|_| ApiError::InternalError)?
    } else if let Some(ref nick) = req.nickname {
        sqlx::query_as::<_, (Uuid, Option<String>, Option<String>)>(
            "SELECT id, nickname, wallet_address FROM users WHERE LOWER(nickname) LIKE LOWER($1) LIMIT 10"
        )
        .bind(format!("{}%", nick))
        .fetch_all(&state.db)
        .await
        .map_err(|_| ApiError::InternalError)?
    } else {
        return Err(ApiError::InvalidRequest("provide address or nickname".into()));
    };

    let mut results = Vec::new();
    for (id, nickname, wallet_address) in rows {
        if id == user.account_id {
            continue; // skip self
        }

        // Check existing contact status
        let contact_status: Option<String> = sqlx::query_scalar(
            r#"
            SELECT status FROM contacts
            WHERE (user_id = $1 AND contact_id = $2)
               OR (user_id = $2 AND contact_id = $1)
            "#
        )
        .bind(user.account_id)
        .bind(id)
        .fetch_optional(&state.db)
        .await
        .map_err(|_| ApiError::InternalError)?;

        results.push(UserLookup {
            id,
            nickname,
            wallet_address,
            identity_display: None, // TODO: PeopleChain lookup
            is_contact: contact_status.as_deref() == Some("accepted"),
            contact_status,
        });
    }

    Ok(Json(results))
}
