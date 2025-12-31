use crate::{config::Config, error::Result};
use aws_credential_types::Credentials;
use aws_sdk_s3::{config::Region, primitives::ByteStream, Client as S3Client};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tracing::{error, info, warn};

#[derive(Debug, Clone)]
pub enum StorageBackend {
    Minio,
    Ipfs,
}

impl From<&str> for StorageBackend {
    fn from(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "ipfs" => StorageBackend::Ipfs,
            _ => StorageBackend::Minio,
        }
    }
}

#[derive(Debug)]
pub struct UploadResult {
    pub url: String,
    pub storage_type: String,
    pub ipfs_cid: Option<String>,
    pub crust_order_id: Option<String>,
    pub pinning_cost: Option<f64>,
}

pub struct StorageService {
    s3_client: Option<S3Client>,
    http: Client,
    config: Config,
}

#[derive(Debug, Deserialize)]
struct IpfsAddResponse {
    #[serde(rename = "Hash")]
    hash: String,
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "Size")]
    size: String,
}

#[derive(Debug, Serialize)]
struct CrustPinRequest {
    cid: String,
    name: String,
}

#[derive(Debug, Deserialize)]
struct CrustPinResponse {
    #[serde(rename = "requestId")]
    request_id: Option<String>,
    status: Option<String>,
}

impl StorageService {
    pub async fn new(config: Config) -> Self {
        let s3_client = if config.default_storage != "ipfs" {
            Some(Self::create_s3_client(&config).await)
        } else {
            None
        };

        Self {
            s3_client,
            http: Client::new(),
            config,
        }
    }

    async fn create_s3_client(config: &Config) -> S3Client {
        let creds = Credentials::new(
            &config.minio_access_key,
            &config.minio_secret_key,
            None,
            None,
            "minio",
        );

        let s3_config = aws_sdk_s3::Config::builder()
            .behavior_version_latest()
            .region(Region::new("us-east-1"))
            .endpoint_url(&config.minio_endpoint)
            .credentials_provider(creds)
            .force_path_style(true)
            .build();

        S3Client::from_conf(s3_config)
    }

    pub async fn ensure_bucket_exists(&self) -> Result<()> {
        let Some(client) = &self.s3_client else {
            return Ok(());
        };

        let bucket = &self.config.s3_bucket;

        match client.head_bucket().bucket(bucket).send().await {
            Ok(_) => Ok(()),
            Err(_) => {
                client
                    .create_bucket()
                    .bucket(bucket)
                    .send()
                    .await
                    .map_err(|_| crate::error::ApiError::InternalError)?;

                let policy = serde_json::json!({
                    "Version": "2012-10-17",
                    "Statement": [{
                        "Effect": "Allow",
                        "Principal": "*",
                        "Action": ["s3:GetObject"],
                        "Resource": [format!("arn:aws:s3:::{}/*", bucket)]
                    }]
                });

                client
                    .put_bucket_policy()
                    .bucket(bucket)
                    .policy(policy.to_string())
                    .send()
                    .await
                    .map_err(|_| crate::error::ApiError::InternalError)?;

                info!("Created bucket: {}", bucket);
                Ok(())
            }
        }
    }

    pub async fn upload(
        &self,
        filename: &str,
        data: &[u8],
        content_type: &str,
        backend: StorageBackend,
    ) -> Result<UploadResult> {
        match backend {
            StorageBackend::Minio => self.upload_minio(filename, data, content_type).await,
            StorageBackend::Ipfs => self.upload_ipfs(filename, data).await,
        }
    }

    async fn upload_minio(
        &self,
        filename: &str,
        data: &[u8],
        content_type: &str,
    ) -> Result<UploadResult> {
        let client = self.s3_client.as_ref().ok_or_else(|| {
            error!("S3 client not initialized");
            crate::error::ApiError::InternalError
        })?;

        client
            .put_object()
            .bucket(&self.config.s3_bucket)
            .key(filename)
            .body(ByteStream::from(data.to_vec()))
            .content_type(content_type)
            .send()
            .await
            .map_err(|e| {
                error!("S3 upload failed: {:?}", e);
                crate::error::ApiError::InternalError
            })?;

        let url = format!("{}/{}", self.config.audio_public_url, filename);

        Ok(UploadResult {
            url,
            storage_type: "minio".to_string(),
            ipfs_cid: None,
            crust_order_id: None,
            pinning_cost: None,
        })
    }

    async fn upload_ipfs(&self, filename: &str, data: &[u8]) -> Result<UploadResult> {
        // upload to local ipfs node via http api
        let form = reqwest::multipart::Form::new().part(
            "file",
            reqwest::multipart::Part::bytes(data.to_vec()).file_name(filename.to_string()),
        );

        let response: reqwest::Response = self
            .http
            .post(format!("{}/api/v0/add", self.config.ipfs_api_url))
            .multipart(form)
            .send()
            .await
            .map_err(|e| {
                error!("IPFS upload failed: {:?}", e);
                crate::error::ApiError::InternalError
            })?;

        if !response.status().is_success() {
            let status = response.status();
            let body: String = response.text().await.unwrap_or_default();
            error!("IPFS API error {}: {}", status, body);
            return Err(crate::error::ApiError::InternalError);
        }

        let ipfs_response: IpfsAddResponse = response.json::<IpfsAddResponse>().await.map_err(|e| {
            error!("Failed to parse IPFS response: {:?}", e);
            crate::error::ApiError::InternalError
        })?;

        let cid = ipfs_response.hash.clone();
        info!("Uploaded to IPFS: {}", cid);

        // pin to crust if configured
        let (crust_order_id, pinning_cost) = if self.config.crust_auth_token.is_some() {
            let size_bytes: u64 = ipfs_response.size.parse().unwrap_or(data.len() as u64);
            match self.pin_to_crust(&cid, filename, size_bytes).await {
                Ok((order_id, cost)) => (Some(order_id), Some(cost)),
                Err(e) => {
                    warn!("Crust pinning failed (content still on IPFS): {:?}", e);
                    (None, None)
                }
            }
        } else {
            (None, None)
        };

        let url = format!("{}/{}", self.config.ipfs_gateway_url, cid);

        Ok(UploadResult {
            url,
            storage_type: "ipfs".to_string(),
            ipfs_cid: Some(cid),
            crust_order_id,
            pinning_cost,
        })
    }

    async fn pin_to_crust(
        &self,
        cid: &str,
        name: &str,
        size_bytes: u64,
    ) -> Result<(String, f64)> {
        let token = self
            .config
            .crust_auth_token
            .as_ref()
            .ok_or(crate::error::ApiError::InternalError)?;

        // crust psa api (pinning service api standard)
        let response = self
            .http
            .post(format!("{}/pins", self.config.crust_api_url))
            .header("Authorization", format!("Bearer {}", token))
            .json(&serde_json::json!({
                "cid": cid,
                "name": name
            }))
            .send()
            .await
            .map_err(|e| {
                error!("Crust API request failed: {:?}", e);
                crate::error::ApiError::InternalError
            })?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            error!("Crust API error {}: {}", status, body);
            return Err(crate::error::ApiError::InternalError);
        }

        let pin_response: CrustPinResponse = response.json().await.map_err(|e| {
            error!("Failed to parse Crust response: {:?}", e);
            crate::error::ApiError::InternalError
        })?;

        let order_id = pin_response.request_id.unwrap_or_else(|| cid.to_string());

        // calculate cost based on size
        let size_mb = size_bytes as f64 / (1024.0 * 1024.0);
        let cost = size_mb * self.config.crust_cost_per_mb;

        info!("Pinned to Crust: {} (order: {}, cost: ${:.6})", cid, order_id, cost);

        Ok((order_id, cost))
    }
}
