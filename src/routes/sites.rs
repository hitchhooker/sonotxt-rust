use axum::{
    extract::{Path, Query, State},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use uuid::Uuid;

use crate::{
    auth::dev::DevUser,
    error::Result,
    services::crawler::crawl_site,
    AppState,
};

#[derive(Debug, Deserialize)]
struct CreateSiteRequest {
    url: String,
    selector: Option<String>,
    auto_crawl: bool,
    crawl_frequency_hours: Option<i32>,
}

#[derive(Debug, Serialize)]
struct Site {
    id: Uuid,
    url: String,
    selector: Option<String>,
    auto_crawl: bool,
    last_crawled_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Serialize)]
struct Content {
    id: Uuid,
    site_id: Uuid,
    url: String,
    text_content: String,
    text_hash: String,
    word_count: i32,
    created_at: chrono::DateTime<chrono::Utc>,
}

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/sites", get(list_sites).post(create_site))
        .route("/sites/:id/crawl", post(trigger_crawl))
        .route("/sites/:id/content", get(get_site_content))
        .route("/content", get(list_content))
        .route("/content/:id/process", post(process_content))
}

async fn create_site(
    State(state): State<Arc<AppState>>,
    user: DevUser,
    Json(req): Json<CreateSiteRequest>,
) -> Result<Json<Site>> {
    let row = sqlx::query!(
        r#"
        INSERT INTO sites (account_id, url, selector, auto_crawl, crawl_frequency_hours)
        VALUES ($1, $2, $3, $4, $5)
        RETURNING id, url, selector, auto_crawl, last_crawled_at
        "#,
        user.id,
        req.url,
        req.selector,
        req.auto_crawl,
        req.crawl_frequency_hours.unwrap_or(24)
    )
    .fetch_one(&state.db)
    .await
    .map_err(|_| crate::error::ApiError::Internal)?;

    Ok(Json(Site {
        id: row.id,
        url: row.url,
        selector: row.selector,
        auto_crawl: row.auto_crawl.unwrap_or(false),
        last_crawled_at: row.last_crawled_at,
    }))
}

async fn list_sites(
    State(state): State<Arc<AppState>>,
    user: DevUser,
) -> Result<Json<Vec<Site>>> {
    let rows = sqlx::query!(
        r#"
        SELECT id, url, selector, auto_crawl, last_crawled_at
        FROM sites
        WHERE account_id = $1
        ORDER BY created_at DESC
        "#,
        user.id
    )
    .fetch_all(&state.db)
    .await
    .map_err(|_| crate::error::ApiError::Internal)?;

    let sites = rows
        .into_iter()
        .map(|row| Site {
            id: row.id,
            url: row.url,
            selector: row.selector,
            auto_crawl: row.auto_crawl.unwrap_or(false),
            last_crawled_at: row.last_crawled_at,
        })
        .collect();

    Ok(Json(sites))
}

async fn trigger_crawl(
    State(state): State<Arc<AppState>>,
    Path(site_id): Path<Uuid>,
    user: DevUser,
) -> Result<Json<serde_json::Value>> {
    let site = sqlx::query!(
        r#"
        SELECT id, url, selector, auto_crawl, last_crawled_at
        FROM sites
        WHERE id = $1 AND account_id = $2
        "#,
        site_id,
        user.id
    )
    .fetch_one(&state.db)
    .await
    .map_err(|_| crate::error::ApiError::NotFound)?;

    let content = crawl_site(&state, &site.url, site.selector.as_deref()).await?;
    
    let content_id = Uuid::new_v4();
    let text_hash = blake3::hash(content.as_bytes()).to_hex().to_string();
    let word_count = content.split_whitespace().count() as i32;

    sqlx::query!(
        r#"
        INSERT INTO content (id, site_id, url, text_content, text_hash, word_count)
        VALUES ($1, $2, $3, $4, $5, $6)
        ON CONFLICT (text_hash) DO NOTHING
        "#,
        content_id,
        site_id,
        site.url,
        content,
        text_hash,
        word_count
    )
    .execute(&state.db)
    .await
    .map_err(|_| crate::error::ApiError::Internal)?;

    sqlx::query!("UPDATE sites SET last_crawled_at = NOW() WHERE id = $1", site_id)
        .execute(&state.db)
        .await
        .map_err(|_| crate::error::ApiError::Internal)?;

    Ok(Json(serde_json::json!({
        "status": "success",
        "content_id": content_id,
        "word_count": word_count
    })))
}

async fn get_site_content(
    State(state): State<Arc<AppState>>,
    Path(site_id): Path<Uuid>,
    _user: DevUser,
) -> Result<Json<Content>> {
    let row = sqlx::query!(
        r#"
        SELECT id, site_id, url, text_content, text_hash, word_count, created_at
        FROM content
        WHERE site_id = $1
        ORDER BY created_at DESC
        LIMIT 1
        "#,
        site_id
    )
    .fetch_one(&state.db)
    .await
    .map_err(|_| crate::error::ApiError::NotFound)?;
    
    Ok(Json(Content {
        id: row.id,
        site_id: row.site_id.unwrap_or(site_id),
        url: row.url,
        text_content: row.text_content,
        text_hash: row.text_hash,
        word_count: row.word_count,
        created_at: row.created_at,
    }))
}

async fn list_content(
    State(state): State<Arc<AppState>>,
    Query(params): Query<std::collections::HashMap<String, String>>,
    user: DevUser,
) -> Result<Json<Vec<serde_json::Value>>> {
    let content = if let Some(site_id) = params.get("site_id") {
        let site_uuid = Uuid::parse_str(site_id).map_err(|_| crate::error::ApiError::Internal)?;
        
        let rows = sqlx::query!(
            r#"
            SELECT c.id, c.url, c.word_count, c.created_at
            FROM content c
            JOIN sites s ON c.site_id = s.id
            WHERE s.account_id = $1 AND c.site_id = $2
            ORDER BY c.created_at DESC
            "#,
            user.id,
            site_uuid
        )
        .fetch_all(&state.db)
        .await
        .map_err(|_| crate::error::ApiError::Internal)?;
        
        rows.into_iter()
            .map(|c| serde_json::json!({
                "id": c.id,
                "url": c.url,
                "word_count": c.word_count,
                "created_at": c.created_at
            }))
            .collect()
    } else {
        let rows = sqlx::query!(
            r#"
            SELECT c.id, c.url, c.word_count, c.created_at
            FROM content c
            JOIN sites s ON c.site_id = s.id
            WHERE s.account_id = $1
            ORDER BY c.created_at DESC
            "#,
            user.id
        )
        .fetch_all(&state.db)
        .await
        .map_err(|_| crate::error::ApiError::Internal)?;
        
        rows.into_iter()
            .map(|c| serde_json::json!({
                "id": c.id,
                "url": c.url,
                "word_count": c.word_count,
                "created_at": c.created_at
            }))
            .collect()
    };

    Ok(Json(content))
}

async fn process_content(
    State(state): State<Arc<AppState>>,
    Path(content_id): Path<Uuid>,
    _user: DevUser,
) -> Result<Json<serde_json::Value>> {
    let content = sqlx::query!(
        r#"
        SELECT text_content, word_count
        FROM content
        WHERE id = $1
        "#,
        content_id
    )
    .fetch_one(&state.db)
    .await
    .map_err(|_| crate::error::ApiError::NotFound)?;

    let job_id = blake3::hash(content.text_content.as_bytes()).to_hex()[..16].to_string();
    let cost = (content.text_content.len() as f64) * state.config.cost_per_char;

    Ok(Json(serde_json::json!({
        "job_id": job_id,
        "estimated_cost": cost,
        "word_count": content.word_count
    })))
}
