use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("Invalid API key")]
    InvalidApiKey,
    
    #[error("Insufficient balance")]
    InsufficientBalance,
    
    #[error("Invalid URL")]
    InvalidUrl,
    
    #[error("Content too large")]
    ContentTooLarge,
    
    #[error("Processing failed")]
    ProcessingFailed,
    
    #[error("Not found")]
    NotFound,
    
    #[error("Unauthorized")]
    Unauthorized,
    
    #[error("Internal error")]
    Internal,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            ApiError::InvalidApiKey => (StatusCode::UNAUTHORIZED, self.to_string()),
            ApiError::InsufficientBalance => (StatusCode::PAYMENT_REQUIRED, self.to_string()),
            ApiError::InvalidUrl => (StatusCode::BAD_REQUEST, self.to_string()),
            ApiError::ContentTooLarge => (StatusCode::PAYLOAD_TOO_LARGE, self.to_string()),
            ApiError::ProcessingFailed => (StatusCode::UNPROCESSABLE_ENTITY, self.to_string()),
            ApiError::NotFound => (StatusCode::NOT_FOUND, self.to_string()),
            ApiError::Unauthorized => (StatusCode::UNAUTHORIZED, self.to_string()),
            ApiError::Internal => (StatusCode::INTERNAL_SERVER_ERROR, "Internal error".to_string()),
        };
        
        (status, Json(json!({ "error": message }))).into_response()
    }
}

pub type Result<T> = std::result::Result<T, ApiError>;
