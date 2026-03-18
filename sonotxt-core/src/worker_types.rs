use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub enum ServiceError {
    Timeout,
    Unavailable,
    Failed(String),
    Cancelled,
}

impl std::fmt::Display for ServiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Timeout => write!(f, "timeout"),
            Self::Unavailable => write!(f, "service unavailable"),
            Self::Failed(msg) => write!(f, "{}", msg),
            Self::Cancelled => write!(f, "cancelled"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct TtsRequest {
    pub text: String,
    pub speaker: String,
    pub language: String,
    pub api_key: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TtsResponse {
    pub audio_data: Vec<u8>,
    pub format: String,
    pub duration_seconds: f64,
    pub runtime_ms: u64,
}

#[derive(Debug, Clone)]
pub struct AsrRequest {
    pub audio_base64: String,
}

#[derive(Debug, Clone)]
pub struct AsrResponse {
    pub text: String,
}

#[derive(Debug, Clone)]
pub struct LlmRequest {
    pub messages: Vec<LlmMessage>,
    pub max_tokens: u32,
    pub temperature: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmMessage {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Clone)]
pub struct LlmResponse {
    pub sentences: Vec<String>,
    pub full_response: String,
    pub tokens: u32,
    pub runtime_ms: u64,
}
