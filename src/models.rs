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

// ~2.5ms per character based on deepinfra Kokoro benchmarks
pub const MS_PER_CHAR: f64 = 2.5;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "status")]
pub enum JobStatus {
    Queued {
        #[serde(skip_serializing_if = "Option::is_none")]
        position: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        estimated_seconds: Option<f64>,
    },
    Processing {
        progress: u8,
        #[serde(skip_serializing_if = "Option::is_none")]
        elapsed_seconds: Option<f64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        estimated_seconds: Option<f64>,
    },
    Complete {
        url: String,
        duration_seconds: f64,
        #[serde(skip_serializing_if = "Option::is_none")]
        runtime_ms: Option<i32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        cost: Option<f64>,
    },
    Failed { reason: String },
}
