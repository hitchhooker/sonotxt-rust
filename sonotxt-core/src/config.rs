/// Shared storage configuration used by both API and worker.
#[derive(Debug, Clone)]
pub struct StorageConfig {
    pub s3_bucket: String,
    pub minio_endpoint: String,
    pub minio_access_key: String,
    pub minio_secret_key: String,
    pub audio_public_url: String,
    pub ipfs_api_url: String,
    pub ipfs_gateway_url: String,
    pub crust_api_url: String,
    pub crust_auth_token: Option<String>,
    pub crust_cost_per_mb: f64,
    pub default_storage: String,
}
