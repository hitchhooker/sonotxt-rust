use crate::{error::Result, services::storage::{StorageBackend, StorageService}, AppState};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::time::{sleep, Duration};
use tracing::{error, info, warn};

const KOKORO_API_URL: &str = "https://api.deepinfra.com/v1/inference/hexgrad/Kokoro-82M";

#[derive(Debug, Serialize)]
struct KokoroRequest {
    text: String,
    output_format: String,
    preset_voice: Vec<String>,
    speed: f64,
}

#[derive(Debug, Deserialize)]
struct KokoroResponse {
    audio: Option<String>,
    inference_status: Option<InferenceStatus>,
    request_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct InferenceStatus {
    status: String,
    runtime_ms: Option<i64>,
    cost: Option<f64>,
}

pub async fn run_worker(state: Arc<AppState>) {
    info!("TTS worker started");

    if state.config.deepinfra_token.is_none() {
        warn!("DEEPINFRA_TOKEN not set - TTS will fail");
    }

    // Initialize storage service
    let storage = StorageService::new(state.config.clone()).await;

    // Ensure bucket exists (for minio)
    if let Err(e) = storage.ensure_bucket_exists().await {
        error!("Failed to create audio bucket: {:?}", e);
    }

    // Recover zombie jobs on startup
    if let Err(e) = recover_zombie_jobs(&state).await {
        error!("Failed to recover zombie jobs: {:?}", e);
    }

    loop {
        if let Err(e) = process_next_job(&state, &storage).await {
            error!("Worker error: {:?}", e);
        }

        sleep(Duration::from_secs(2)).await;
    }
}


async fn recover_zombie_jobs(state: &Arc<AppState>) -> Result<()> {
    let recovered = sqlx::query!(
        r#"
        UPDATE jobs
        SET status = 'queued'
        WHERE status = 'processing'
        AND created_at < NOW() - INTERVAL '5 minutes'
        "#
    )
    .execute(&state.db)
    .await
    .map_err(|_| crate::error::ApiError::InternalError)?;

    if recovered.rows_affected() > 0 {
        warn!("Recovered {} zombie jobs", recovered.rows_affected());
    }

    Ok(())
}

async fn process_next_job(state: &Arc<AppState>, storage: &StorageService) -> Result<()> {
    let job = sqlx::query!(
        r#"
        UPDATE jobs
        SET status = 'processing', started_at = NOW()
        WHERE id = (
            SELECT id FROM jobs
            WHERE status = 'queued'
            ORDER BY created_at
            LIMIT 1
            FOR UPDATE SKIP LOCKED
        )
        RETURNING id, content_id, api_key, text_content, voice, cost, storage_type
        "#
    )
    .fetch_optional(&state.db)
    .await
    .map_err(|_| crate::error::ApiError::InternalError)?;

    let Some(job) = job else {
        return Ok(());
    };

    info!("Processing job: {}", job.id);

    let text = if let Some(ref t) = job.text_content {
        t.clone()
    } else if let Some(content_id) = job.content_id {
        let content = sqlx::query!(
            "SELECT text_content FROM content WHERE id = $1",
            content_id
        )
        .fetch_one(&state.db)
        .await
        .map_err(|_| crate::error::ApiError::InternalError)?;
        content.text_content
    } else {
        error!("Job {} has no content", job.id);
        mark_job_failed(state, &job.id, "No content provided").await?;
        return Ok(());
    };

    let voice = job.voice.as_str();

    // Determine storage backend (job preference or default)
    let storage_type = job.storage_type.as_deref().unwrap_or(&state.config.default_storage);
    let backend = StorageBackend::from(storage_type);

    // Call Kokoro TTS
    match generate_tts_kokoro(state, &text, voice).await {
        Ok(result) => {
            let filename = format!("{}.mp3", job.id);
            match storage.upload(&filename, &result.audio_data, "audio/mpeg", backend).await {
                Ok(upload_result) => {
                    let runtime_ms = result.runtime_ms.map(|ms| ms as i32);
                    let pinning_cost = upload_result.pinning_cost;

                    sqlx::query!(
                        r#"
                        UPDATE jobs
                        SET status = 'completed',
                            audio_url = $1,
                            duration_seconds = $2,
                            actual_runtime_ms = $3,
                            deepinfra_cost = $4,
                            deepinfra_request_id = $5,
                            storage_type = $6,
                            ipfs_cid = $7,
                            crust_order_id = $8,
                            pinning_cost = $9,
                            completed_at = NOW()
                        WHERE id = $10
                        "#,
                        upload_result.url,
                        result.duration_seconds,
                        runtime_ms,
                        result.cost,
                        result.request_id,
                        upload_result.storage_type,
                        upload_result.ipfs_cid,
                        upload_result.crust_order_id,
                        pinning_cost,
                        job.id
                    )
                    .execute(&state.db)
                    .await
                    .map_err(|_| crate::error::ApiError::InternalError)?;

                    let storage_info = if let Some(cid) = &upload_result.ipfs_cid {
                        format!("ipfs:{}", cid)
                    } else {
                        "minio".to_string()
                    };

                    info!(
                        "Job {} completed: {:.1}s audio, {} bytes, runtime {}ms, cost ${:.6}, storage: {}",
                        job.id,
                        result.duration_seconds,
                        result.audio_data.len(),
                        result.runtime_ms.unwrap_or(0),
                        result.cost.unwrap_or(0.0),
                        storage_info
                    );
                }
                Err(e) => {
                    error!("Failed to upload audio for job {}: {:?}", job.id, e);
                    mark_job_failed(state, &job.id, "Failed to upload audio").await?;
                }
            }
        }
        Err(e) => {
            error!("TTS generation failed for job {}: {:?}", job.id, e);
            mark_job_failed(state, &job.id, &format!("TTS failed: {:?}", e)).await?;
        }
    }

    Ok(())
}

struct TtsResult {
    audio_data: Vec<u8>,
    duration_seconds: f64,
    runtime_ms: Option<i64>,
    cost: Option<f64>,
    request_id: Option<String>,
}

async fn generate_tts_kokoro(state: &AppState, text: &str, voice: &str) -> Result<TtsResult> {
    let token = state
        .config
        .deepinfra_token
        .as_ref()
        .ok_or(crate::error::ApiError::InternalError)?;

    let request = KokoroRequest {
        text: text.to_string(),
        output_format: "mp3".to_string(),
        preset_voice: vec![voice.to_string()],
        speed: 1.0,
    };

    let response = state
        .http
        .post(KOKORO_API_URL)
        .header("Authorization", format!("bearer {}", token))
        .header("Content-Type", "application/json")
        .json(&request)
        .send()
        .await
        .map_err(|e| {
            error!("Kokoro API request failed: {:?}", e);
            crate::error::ApiError::ProcessingFailed
        })?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        error!("Kokoro API error {}: {}", status, body);
        return Err(crate::error::ApiError::ProcessingFailed);
    }

    let kokoro_response: KokoroResponse = response.json().await.map_err(|e| {
        error!("Failed to parse Kokoro response: {:?}", e);
        crate::error::ApiError::ProcessingFailed
    })?;

    // Extract deepinfra stats
    let (runtime_ms, cost) = kokoro_response
        .inference_status
        .map(|s| (s.runtime_ms, s.cost))
        .unwrap_or((None, None));

    let audio_data_url = kokoro_response
        .audio
        .ok_or(crate::error::ApiError::ProcessingFailed)?;

    // Strip data URL prefix (e.g., "data:audio/wav;base64," or "data:audio/mp3;base64,")
    let audio_base64 = audio_data_url
        .split(',')
        .nth(1)
        .unwrap_or(&audio_data_url);

    let audio_data = BASE64.decode(audio_base64).map_err(|e| {
        error!("Failed to decode audio base64: {:?}", e);
        crate::error::ApiError::ProcessingFailed
    })?;

    // Estimate duration based on audio size (MP3 ~64kbps = 8KB/sec for 24kHz mono)
    let duration_seconds = audio_data.len() as f64 / 8000.0;

    Ok(TtsResult {
        audio_data,
        duration_seconds,
        runtime_ms,
        cost,
        request_id: kokoro_response.request_id,
    })
}

async fn mark_job_failed(state: &Arc<AppState>, job_id: &str, reason: &str) -> Result<()> {
    sqlx::query!(
        r#"
        UPDATE jobs
        SET status = 'failed', error_message = $1, completed_at = NOW()
        WHERE id = $2
        "#,
        reason,
        job_id
    )
    .execute(&state.db)
    .await
    .map_err(|_| crate::error::ApiError::InternalError)?;

    Ok(())
}
