use crate::{error::Result, services::storage::{StorageBackend, StorageService}, AppState};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::process::Command;
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
        RETURNING id, content_id, api_key, text_content, voice, cost, storage_type, engine
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
    let engine = job.engine.as_deref().unwrap_or("kokoro");

    // Determine storage backend (job preference or default)
    let storage_type = job.storage_type.as_deref().unwrap_or(&state.config.default_storage);
    let backend = StorageBackend::from(storage_type);

    // Route to appropriate TTS engine
    let tts_result = match engine {
        "vibevoice" | "vibevoice-streaming" => {
            info!("Using VibeVoice TTS engine: {}", engine);
            generate_tts_vibevoice(state, &text, voice, engine).await
        }
        _ => {
            info!("Using Kokoro TTS engine (DeepInfra)");
            generate_tts_kokoro(state, &text, voice).await
        }
    };

    match tts_result {
        Ok(result) => {
            // opus from API, wrap in ogg container if needed
            let (audio_data, filename, content_type) = if result.format == "opus" {
                // wrap raw opus in ogg container for browser compatibility
                match wrap_opus_in_ogg(&result.audio_data).await {
                    Ok(ogg_data) => (ogg_data, format!("{}.ogg", job.id), "audio/ogg"),
                    Err(e) => {
                        warn!("ogg wrapping failed, storing raw opus: {:?}", e);
                        (result.audio_data.clone(), format!("{}.opus", job.id), "audio/opus")
                    }
                }
            } else {
                // fallback for other formats
                let ext = if result.format == "mp3" { "mp3" } else { "wav" };
                let ct = if result.format == "mp3" { "audio/mpeg" } else { "audio/wav" };
                (result.audio_data.clone(), format!("{}.{}", job.id, ext), ct)
            };

            match storage.upload(&filename, &audio_data, content_type, backend).await {
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
                        "Job {} completed: {:.1}s audio, {} bytes ({}), runtime {}ms, cost ${:.6}, storage: {}",
                        job.id,
                        result.duration_seconds,
                        audio_data.len(),
                        result.format,
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
    format: String,
    duration_seconds: f64,
    runtime_ms: Option<i64>,
    cost: Option<f64>,
    request_id: Option<String>,
}

/// wrap raw opus in ogg container using ffmpeg (for browser compatibility)
async fn wrap_opus_in_ogg(opus_data: &[u8]) -> Result<Vec<u8>> {
    use std::process::Stdio;
    use tokio::io::AsyncWriteExt;

    // DeepInfra opus output might already be in ogg container
    // Check for OggS magic bytes
    if opus_data.len() > 4 && &opus_data[0..4] == b"OggS" {
        // Already in ogg container, return as-is
        return Ok(opus_data.to_vec());
    }

    // Wrap raw opus in ogg container
    let mut child = Command::new("ffmpeg")
        .args([
            "-f", "opus",        // input format: raw opus
            "-i", "pipe:0",      // input from stdin
            "-c:a", "copy",      // copy codec (no re-encoding)
            "-f", "ogg",         // output format: ogg container
            "pipe:1"             // output to stdout
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            error!("failed to spawn ffmpeg: {:?}", e);
            crate::error::ApiError::ProcessingFailed
        })?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(opus_data).await.map_err(|e| {
            error!("failed to write to ffmpeg stdin: {:?}", e);
            crate::error::ApiError::ProcessingFailed
        })?;
    }

    let output = child.wait_with_output().await.map_err(|e| {
        error!("ffmpeg process failed: {:?}", e);
        crate::error::ApiError::ProcessingFailed
    })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        error!("ffmpeg ogg wrapping failed: {}", stderr);
        return Err(crate::error::ApiError::ProcessingFailed);
    }

    Ok(output.stdout)
}

async fn generate_tts_kokoro(state: &AppState, text: &str, voice: &str) -> Result<TtsResult> {
    let token = state
        .config
        .deepinfra_token
        .as_ref()
        .ok_or(crate::error::ApiError::InternalError)?;

    // request opus for best compression, fallback handled by API
    let output_format = "opus";

    let request = KokoroRequest {
        text: text.to_string(),
        output_format: output_format.to_string(),
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

    // Estimate duration based on audio size
    // opus ~64kbps = 8KB/sec, mp3 similar
    let duration_seconds = audio_data.len() as f64 / 8000.0;

    Ok(TtsResult {
        audio_data,
        format: output_format.to_string(),
        duration_seconds,
        runtime_ms,
        cost,
        request_id: kokoro_response.request_id,
    })
}

async fn generate_tts_vibevoice(
    state: &AppState,
    text: &str,
    voice: &str,
    engine: &str,
) -> Result<TtsResult> {
    let vibevoice_url = state
        .config
        .vibevoice_url
        .as_ref()
        .ok_or_else(|| {
            error!("VIBEVOICE_URL not configured");
            crate::error::ApiError::InternalError
        })?;

    #[derive(serde::Serialize)]
    struct VibeRequest {
        text: String,
        voice: String,
        speed: f32,
        output_format: String,
    }

    #[derive(serde::Deserialize)]
    struct VibeResponse {
        audio_base64: String,
        sample_rate: i32,
        duration_seconds: f64,
        format: String,
    }

    let start = std::time::Instant::now();

    let request = VibeRequest {
        text: text.to_string(),
        voice: voice.to_string(),
        speed: 1.0,
        output_format: "wav".to_string(),
    };

    let response = state
        .http
        .post(format!("{}/synthesize", vibevoice_url))
        .header("Content-Type", "application/json")
        .json(&request)
        .timeout(std::time::Duration::from_secs(180))
        .send()
        .await
        .map_err(|e| {
            error!("VibeVoice API request failed: {:?}", e);
            crate::error::ApiError::ProcessingFailed
        })?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        error!("VibeVoice API error {}: {}", status, body);
        return Err(crate::error::ApiError::ProcessingFailed);
    }

    let vibe_response: VibeResponse = response.json().await.map_err(|e| {
        error!("Failed to parse VibeVoice response: {:?}", e);
        crate::error::ApiError::ProcessingFailed
    })?;

    let runtime_ms = start.elapsed().as_millis() as i64;

    // decode hex audio data
    let audio_data = hex::decode(&vibe_response.audio_base64).map_err(|e| {
        error!("Failed to decode audio hex: {:?}", e);
        crate::error::ApiError::ProcessingFailed
    })?;

    info!(
        "VibeVoice synthesis completed: {:.1}s audio, {} bytes, {}ms runtime",
        vibe_response.duration_seconds,
        audio_data.len(),
        runtime_ms
    );

    Ok(TtsResult {
        audio_data,
        format: vibe_response.format,
        duration_seconds: vibe_response.duration_seconds,
        runtime_ms: Some(runtime_ms),
        cost: None, // self-hosted, no external cost
        request_id: None,
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
