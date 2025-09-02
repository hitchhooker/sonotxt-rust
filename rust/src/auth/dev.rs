use axum::{
    async_trait,
    extract::FromRequestParts,
    http::request::Parts,
};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct DevUser {
    pub id: Uuid,
    pub email: String,
}

#[async_trait]
impl FromRequestParts<std::sync::Arc<crate::AppState>> for DevUser {
    type Rejection = crate::error::ApiError;

    async fn from_request_parts(
        _parts: &mut Parts,
        _state: &std::sync::Arc<crate::AppState>,
    ) -> Result<Self, Self::Rejection> {
        // Always return test user in dev mode
        Ok(DevUser {
            id: Uuid::parse_str("123e4567-e89b-12d3-a456-426614174000").unwrap(),
            email: "dev@sonotxt.local".to_string(),
        })
    }
}
