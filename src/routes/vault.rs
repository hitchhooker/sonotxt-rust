// Encrypted vault storage for paid users
// Server stores encrypted blobs - cannot decrypt them
// Client encrypts with PRF-derived key before upload

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
    Json, Router,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use std::sync::Arc;
use uuid::Uuid;

use crate::{auth::AuthenticatedUser, error::Result, AppState};

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/vault", get(list_items))
        .route("/vault", post(upload_encrypted))
        .route("/vault/{id}", get(download_encrypted))
        .route("/vault/{id}", delete(delete_item))
        .route("/vault/{id}/publish", post(publish_item))
}

#[derive(Debug, Serialize)]
struct VaultItem {
    id: String,
    filename: String,
    size_bytes: i64,
    content_type: String,
    is_public: bool,
    public_url: Option<String>,
    created_at: String,
}

#[derive(Debug, FromRow)]
struct VaultItemRow {
    id: String,
    filename: String,
    size_bytes: i64,
    content_type: String,
    is_public: bool,
    public_url: Option<String>,
    created_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
struct VaultListResponse {
    items: Vec<VaultItem>,
    total_bytes: i64,
    quota_bytes: i64,
}

async fn list_items(
    State(state): State<Arc<AppState>>,
    user: AuthenticatedUser,
) -> Result<Json<VaultListResponse>> {
    let items: Vec<VaultItemRow> = sqlx::query_as(
        r#"SELECT id, filename, size_bytes, content_type, is_public, public_url, created_at
           FROM vault_items
           WHERE account_id = $1
           ORDER BY created_at DESC
           LIMIT 100"#,
    )
    .bind(user.account_id)
    .fetch_all(&state.db)
    .await?;

    let total_bytes: i64 = items.iter().map(|i| i.size_bytes).sum();

    // 1GB quota for paid users
    let quota_bytes: i64 = 1024 * 1024 * 1024;

    let vault_items: Vec<VaultItem> = items
        .into_iter()
        .map(|i| VaultItem {
            id: i.id,
            filename: i.filename,
            size_bytes: i.size_bytes,
            content_type: i.content_type,
            is_public: i.is_public,
            public_url: i.public_url,
            created_at: i.created_at.to_rfc3339(),
        })
        .collect();

    Ok(Json(VaultListResponse {
        items: vault_items,
        total_bytes,
        quota_bytes,
    }))
}

#[derive(Debug, Deserialize)]
struct UploadQuery {
    filename: String,
    #[serde(default = "default_content_type")]
    content_type: String,
}

fn default_content_type() -> String {
    "application/octet-stream".to_string()
}

#[derive(Debug, Serialize)]
struct UploadResponse {
    id: String,
    size_bytes: i64,
}

async fn upload_encrypted(
    State(state): State<Arc<AppState>>,
    user: AuthenticatedUser,
    axum::extract::Query(query): axum::extract::Query<UploadQuery>,
    body: Bytes,
) -> Result<Json<UploadResponse>> {
    let size_bytes = body.len() as i64;

    // check quota (1GB)
    let used: Option<i64> = sqlx::query_scalar(
        "SELECT COALESCE(SUM(size_bytes), 0) FROM vault_items WHERE account_id = $1",
    )
    .bind(user.account_id)
    .fetch_one(&state.db)
    .await?;

    let quota: i64 = 1024 * 1024 * 1024; // 1GB
    if used.unwrap_or(0) + size_bytes > quota {
        return Err(crate::error::ApiError::QuotaExceeded);
    }

    // max single file 100MB
    if size_bytes > 100 * 1024 * 1024 {
        return Err(crate::error::ApiError::ContentTooLarge);
    }

    let id = Uuid::new_v4().to_string();
    let storage_key = format!("vault/{}/{}", user.account_id, id);

    // upload to s3/minio
    let storage = crate::services::storage::StorageService::new(state.config.clone()).await;
    storage
        .upload(&storage_key, &body, &query.content_type, crate::services::storage::StorageBackend::Minio)
        .await?;

    // store metadata
    sqlx::query(
        r#"INSERT INTO vault_items (id, account_id, filename, size_bytes, content_type, storage_key, is_public)
           VALUES ($1, $2, $3, $4, $5, $6, FALSE)"#,
    )
    .bind(&id)
    .bind(user.account_id)
    .bind(&query.filename)
    .bind(size_bytes)
    .bind(&query.content_type)
    .bind(&storage_key)
    .execute(&state.db)
    .await?;

    Ok(Json(UploadResponse { id, size_bytes }))
}

#[derive(Debug, FromRow)]
struct VaultItemDownload {
    storage_key: String,
    content_type: String,
    filename: String,
}

async fn download_encrypted(
    State(state): State<Arc<AppState>>,
    user: AuthenticatedUser,
    Path(id): Path<String>,
) -> Result<Response> {
    let item: VaultItemDownload = sqlx::query_as(
        "SELECT storage_key, content_type, filename FROM vault_items WHERE id = $1 AND account_id = $2",
    )
    .bind(&id)
    .bind(user.account_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or(crate::error::ApiError::NotFound)?;

    // fetch from minio
    let minio_url = format!(
        "{}/{}/{}",
        state.config.minio_endpoint,
        state.config.s3_bucket,
        item.storage_key
    );

    let resp = state.http.get(&minio_url).send().await
        .map_err(|_| crate::error::ApiError::InternalError)?;

    if !resp.status().is_success() {
        return Err(crate::error::ApiError::NotFound);
    }

    let bytes = resp.bytes().await
        .map_err(|_| crate::error::ApiError::InternalError)?;

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, item.content_type)
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{}\"", item.filename),
        )
        .body(axum::body::Body::from(bytes))
        .unwrap())
}

#[derive(Debug, FromRow)]
struct VaultStorageKey {
    storage_key: String,
}

async fn delete_item(
    State(state): State<Arc<AppState>>,
    user: AuthenticatedUser,
    Path(id): Path<String>,
) -> Result<StatusCode> {
    let item: VaultStorageKey = sqlx::query_as(
        "SELECT storage_key FROM vault_items WHERE id = $1 AND account_id = $2",
    )
    .bind(&id)
    .bind(user.account_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or(crate::error::ApiError::NotFound)?;

    // delete from storage (best effort)
    let minio_url = format!(
        "{}/{}/{}",
        state.config.minio_endpoint,
        state.config.s3_bucket,
        item.storage_key
    );
    let _ = state.http.delete(&minio_url).send().await;

    // delete from db
    sqlx::query("DELETE FROM vault_items WHERE id = $1 AND account_id = $2")
        .bind(&id)
        .bind(user.account_id)
        .execute(&state.db)
        .await?;

    Ok(StatusCode::NO_CONTENT)
}

// Publishing costs: $0.10/MB for distribution (CDN, bandwidth, storage replication)
const PUBLISH_COST_PER_MB: f64 = 0.10;

#[derive(Debug, Deserialize)]
struct PublishRequest {
    // encrypted data to decrypt and publish (optional - if not provided, item stays encrypted)
    decrypted_data: Option<String>, // base64 encoded decrypted audio
    // storage backend for public file
    #[serde(default)]
    storage: Option<String>, // "minio" or "ipfs" (ipfs costs more for pinning)
}

#[derive(Debug, Serialize)]
struct PublishResponse {
    public_url: String,
    cost: f64,
    ipfs_cid: Option<String>,
}

#[derive(Debug, FromRow)]
struct VaultItemPublish {
    storage_key: String,
    content_type: String,
    #[allow(dead_code)]
    filename: String,
    size_bytes: i64,
}

async fn publish_item(
    State(state): State<Arc<AppState>>,
    user: AuthenticatedUser,
    Path(id): Path<String>,
    Json(req): Json<PublishRequest>,
) -> Result<Json<PublishResponse>> {
    let item: VaultItemPublish = sqlx::query_as(
        "SELECT storage_key, content_type, filename, size_bytes FROM vault_items WHERE id = $1 AND account_id = $2",
    )
    .bind(&id)
    .bind(user.account_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or(crate::error::ApiError::NotFound)?;

    // calculate cost based on size
    let size_mb = item.size_bytes as f64 / (1024.0 * 1024.0);
    let publish_cost = size_mb * PUBLISH_COST_PER_MB;

    // check balance
    let balance: Option<f64> = sqlx::query_scalar(
        "SELECT balance FROM account_credits WHERE account_id = $1",
    )
    .bind(user.account_id)
    .fetch_optional(&state.db)
    .await?;

    if balance.unwrap_or(0.0) < publish_cost {
        return Err(crate::error::ApiError::InsufficientBalance);
    }

    // deduct balance
    sqlx::query("UPDATE account_credits SET balance = balance - $1 WHERE account_id = $2")
        .bind(publish_cost)
        .bind(user.account_id)
        .execute(&state.db)
        .await?;

    let storage_backend = match req.storage.as_deref() {
        Some("ipfs") => crate::services::storage::StorageBackend::Ipfs,
        _ => crate::services::storage::StorageBackend::Minio,
    };

    let public_key = format!("public/{}", id);
    let public_url: String;
    let mut ipfs_cid: Option<String> = None;

    if let Some(decrypted_b64) = req.decrypted_data {
        // client provided decrypted data - upload that as public
        use base64::Engine;
        let decrypted = base64::engine::general_purpose::STANDARD
            .decode(&decrypted_b64)
            .map_err(|_| crate::error::ApiError::InvalidRequestError)?;

        let storage = crate::services::storage::StorageService::new(state.config.clone()).await;
        let upload_result = storage
            .upload(&public_key, &decrypted, &item.content_type, storage_backend)
            .await?;

        public_url = upload_result.url;
        ipfs_cid = upload_result.ipfs_cid;
    } else {
        // just make existing encrypted file public (client will need to decrypt)
        public_url = format!("{}/{}/{}", state.config.audio_public_url, state.config.s3_bucket, item.storage_key);
    }

    sqlx::query("UPDATE vault_items SET is_public = TRUE, public_url = $1, ipfs_cid = $2 WHERE id = $3")
        .bind(&public_url)
        .bind(&ipfs_cid)
        .bind(&id)
        .execute(&state.db)
        .await?;

    Ok(Json(PublishResponse {
        public_url,
        cost: publish_cost,
        ipfs_cid,
    }))
}
