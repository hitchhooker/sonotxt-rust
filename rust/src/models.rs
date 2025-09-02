use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKey {
    pub key: String,
    pub account_id: Uuid,
    pub balance: f64,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub monthly_usage: f64,
}

impl ApiKey {
    pub fn new(account_id: Uuid, balance: f64) -> Self {
        Self {
            key: Uuid::new_v4().to_string(),
            account_id,
            balance,
            created_at: Utc::now(),
            monthly_usage: 0.0,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ProcessRequest {
    pub url: String,
    #[serde(default)]
    pub selector: Option<String>,
    #[serde(default)]
    pub voice: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ProcessResponse {
    pub job_id: String,
    pub status: JobStatus,
    pub estimated_cost: f64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "status")]
pub enum JobStatus {
    Queued,
    Processing { progress: u8 },
    Complete { url: String, duration_seconds: f64 },
    Failed { reason: String },
}
