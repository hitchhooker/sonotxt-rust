use serde::{Deserialize, Serialize};

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
