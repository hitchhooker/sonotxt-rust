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
use crate::{error::ApiError, models::ApiKey, AppState};

pub struct AuthenticatedUser(pub ApiKey);

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

        let mut redis = state.redis.clone();
        let key_data: Option<String> = redis::cmd("GET")
            .arg(format!("apikey:{}", bearer.token()))
            .query_async(&mut redis)
            .await
            .map_err(|_| ApiError::Internal)?;

        match key_data {
            Some(json) => {
                let api_key: ApiKey = serde_json::from_str(&json)
                    .map_err(|_| ApiError::InvalidApiKey)?;
                Ok(AuthenticatedUser(api_key))
            }
            None => Err(ApiError::InvalidApiKey),
        }
    }
}
