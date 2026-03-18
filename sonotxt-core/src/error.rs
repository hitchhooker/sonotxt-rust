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

    #[error("Free tier limit exceeded: {remaining} of {limit} chars remaining today")]
    FreeTierLimitExceeded { remaining: i32, limit: i32 },

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

    #[error("Invalid credentials")]
    InvalidCredentials,

    #[error("Rate limited - please try again later")]
    RateLimited,

    #[error("Internal error")]
    InternalError,

    #[error("Internal error: {0}")]
    Internal(String),

    #[error("Invalid request")]
    InvalidRequestError,

    #[error("Invalid request: {0}")]
    InvalidRequest(String),

    #[error("Storage quota exceeded")]
    QuotaExceeded,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, body) = match &self {
            ApiError::InvalidApiKey => (StatusCode::UNAUTHORIZED, json!({ "error": self.to_string() })),
            ApiError::InsufficientBalance => (StatusCode::PAYMENT_REQUIRED, json!({ "error": self.to_string() })),
            ApiError::FreeTierLimitExceeded { remaining, limit } => (
                StatusCode::TOO_MANY_REQUESTS,
                json!({
                    "error": self.to_string(),
                    "remaining": remaining,
                    "limit": limit,
                    "hint": "sign up for an api key at app.sonotxt.com for more"
                })
            ),
            ApiError::InvalidUrl => (StatusCode::BAD_REQUEST, json!({ "error": self.to_string() })),
            ApiError::ContentTooLarge => (StatusCode::PAYLOAD_TOO_LARGE, json!({ "error": self.to_string() })),
            ApiError::ProcessingFailed => (StatusCode::UNPROCESSABLE_ENTITY, json!({ "error": self.to_string() })),
            ApiError::NotFound => (StatusCode::NOT_FOUND, json!({ "error": self.to_string() })),
            ApiError::Unauthorized => (StatusCode::UNAUTHORIZED, json!({ "error": self.to_string() })),
            ApiError::InvalidCredentials => (StatusCode::UNAUTHORIZED, json!({ "error": self.to_string() })),
            ApiError::RateLimited => (StatusCode::TOO_MANY_REQUESTS, json!({ "error": self.to_string() })),
            ApiError::InternalError => (StatusCode::INTERNAL_SERVER_ERROR, json!({ "error": self.to_string() })),
            ApiError::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, json!({ "error": self.to_string() })),
            ApiError::InvalidRequestError => (StatusCode::BAD_REQUEST, json!({ "error": self.to_string() })),
            ApiError::InvalidRequest(_) => (StatusCode::BAD_REQUEST, json!({ "error": self.to_string() })),
            ApiError::QuotaExceeded => (StatusCode::INSUFFICIENT_STORAGE, json!({ "error": self.to_string() })),
        };

        (status, Json(body)).into_response()
    }
}

impl From<sqlx::Error> for ApiError {
    fn from(_: sqlx::Error) -> Self {
        ApiError::InternalError
    }
}

pub type Result<T> = std::result::Result<T, ApiError>;
