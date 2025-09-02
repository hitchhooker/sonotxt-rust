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
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize)]
pub struct StackUser {
    pub id: String,
    pub email: Option<String>,
    pub display_name: Option<String>,
}

pub struct StackAuthUser(pub StackUser);

#[async_trait]
impl FromRequestParts<std::sync::Arc<crate::AppState>> for StackAuthUser {
    type Rejection = crate::error::ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &std::sync::Arc<crate::AppState>,
    ) -> Result<Self, Self::Rejection> {
        let TypedHeader(Authorization(bearer)) = parts
            .extract::<TypedHeader<Authorization<Bearer>>>()
            .await
            .map_err(|_| crate::error::ApiError::InvalidApiKey)?;

        // Verify with Stack Auth API
        let client = reqwest::Client::new();
        let res = client
            .get(format!("https://api.stack-auth.com/api/v1/projects/{}/users/me", 
                env!("STACK_PROJECT_ID")))
            .bearer_auth(bearer.token())
            .send()
            .await
            .map_err(|_| crate::error::ApiError::Internal)?;

        if !res.status().is_success() {
            return Err(crate::error::ApiError::InvalidApiKey);
        }

        let user: StackUser = res.json().await
            .map_err(|_| crate::error::ApiError::Internal)?;

        Ok(StackAuthUser(user))
    }
}
