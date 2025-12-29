use axum::{
    async_trait,
    extract::FromRequestParts,
    http::request::Parts,
    RequestPartsExt,
};
use axum_extra::{
    headers::{authorization::Bearer, Authorization},
    TypedHeader,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{error::ApiError, AppState};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKey {
    pub key: String,
    pub account_id: Uuid,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub revoked: bool,
}

#[derive(Debug, Clone)]
pub struct AuthenticatedUser {
    pub account_id: Uuid,
    pub api_key: String,
}

#[async_trait]
impl FromRequestParts<std::sync::Arc<AppState>> for AuthenticatedUser {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &std::sync::Arc<AppState>,
    ) -> Result<Self, Self::Rejection> {
        let TypedHeader(Authorization(bearer)) = parts
            .extract::<TypedHeader<Authorization<Bearer>>>()
            .await
            .map_err(|_| ApiError::InvalidApiKey)?;

        let token = bearer.token();

        // Check Redis cache first
        let mut redis = state.redis.clone();
        let cached: Option<String> = redis::cmd("GET")
            .arg(format!("apikey:{}", token))
            .query_async(&mut redis)
            .await
            .map_err(|_| ApiError::InternalError)?;

        if let Some(json) = cached {
            let api_key: ApiKey = serde_json::from_str(&json)
                .map_err(|_| ApiError::InvalidApiKey)?;

            if api_key.revoked {
                return Err(ApiError::InvalidApiKey);
            }

            return Ok(AuthenticatedUser {
                account_id: api_key.account_id,
                api_key: api_key.key,
            });
        }

        // Fall back to database
        let row = sqlx::query!(
            r#"
            SELECT account_id, revoked
            FROM api_keys
            WHERE key = $1
            "#,
            token
        )
        .fetch_optional(&state.db)
        .await
        .map_err(|_| ApiError::InternalError)?;

        match row {
            Some(r) if !r.revoked => {
                // Cache in Redis for 5 minutes
                let api_key = ApiKey {
                    key: token.to_string(),
                    account_id: r.account_id,
                    created_at: Utc::now(),
                    revoked: false,
                };

                let _ = redis::cmd("SETEX")
                    .arg(format!("apikey:{}", token))
                    .arg(300)
                    .arg(serde_json::to_string(&api_key).unwrap_or_default())
                    .query_async::<_, ()>(&mut redis)
                    .await;

                Ok(AuthenticatedUser {
                    account_id: r.account_id,
                    api_key: token.to_string(),
                })
            }
            _ => Err(ApiError::InvalidApiKey),
        }
    }
}
