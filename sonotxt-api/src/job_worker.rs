//! TTS job processor — polls DB queue, synthesizes via worker pool, uploads to storage.
//!
//! Replaces the old monolithic worker.rs that was split into sonotxt-worker.
//! This runs inside sonotxt-api and uses the WorkerPool (HTTP to speech service)
//! instead of calling local python directly.

use crate::AppState;
use sonotxt_core::{StorageBackend, StorageService};
use std::sync::Arc;
use tokio::time::{sleep, Duration};
use tracing::{error, info, warn};

pub async fn run(state: Arc<AppState>) {
    info!("job worker: polling DB for queued TTS jobs");

    let storage = StorageService::new(state.config.storage_config()).await;
    if let Err(e) = storage.ensure_bucket_exists().await {
        error!("failed to create audio bucket: {:?}", e);
    }

    // Recover zombie jobs on startup
    if let Err(e) = recover_zombies(&state).await {
        error!("failed to recover zombie jobs: {:?}", e);
    }

    loop {
        match process_next(&state, &storage).await {
            Ok(true) => continue, // processed a job, check for more immediately
            Ok(false) => {}       // no jobs, wait
            Err(e) => error!("job worker error: {:?}", e),
        }
        sleep(Duration::from_secs(2)).await;
    }
}

async fn recover_zombies(state: &AppState) -> Result<(), sqlx::Error> {
    let recovered = sqlx::query(
        "UPDATE jobs SET status = 'queued' WHERE status = 'processing' AND created_at < NOW() - INTERVAL '5 minutes'"
    )
    .execute(&state.db)
    .await?;

    if recovered.rows_affected() > 0 {
        warn!("recovered {} zombie jobs", recovered.rows_affected());
    }
    Ok(())
}

#[derive(sqlx::FromRow)]
struct JobRow {
    id: String,
    text_content: Option<String>,
    voice: String,
    storage_type: Option<String>,
    content_id: Option<i64>,
}

/// Returns true if a job was processed.
async fn process_next(state: &AppState, storage: &StorageService) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    let job: Option<JobRow> = sqlx::query_as(
        r#"
        UPDATE jobs
        SET status = 'processing', started_at = NOW()
        WHERE id = (
            SELECT id FROM jobs
            WHERE status = 'queued'
            ORDER BY priority DESC, created_at ASC
            LIMIT 1
            FOR UPDATE SKIP LOCKED
        )
        RETURNING id, text_content, voice, storage_type, content_id
        "#
    )
    .fetch_optional(&state.db)
    .await?;

    let Some(job) = job else {
        return Ok(false);
    };

    info!("processing job: {}", job.id);

    // Get text content
    let text = if let Some(ref t) = job.text_content {
        t.clone()
    } else if let Some(content_id) = job.content_id {
        let row: (String,) = sqlx::query_as("SELECT text_content FROM content WHERE id = $1")
            .bind(content_id)
            .fetch_one(&state.db)
            .await?;
        row.0
    } else {
        mark_failed(&state.db, &job.id, "No content").await;
        return Ok(true);
    };

    let voice = &job.voice;
    let storage_type = job.storage_type.as_deref().unwrap_or(&state.config.default_storage);
    let backend = StorageBackend::from(storage_type);

    // Route through worker pool
    let pool = state.workers.as_ref().ok_or("no workers")?;
    let tts_req = crate::services::worker_pool::TtsRequest {
        text: text.clone(),
        speaker: voice.to_string(),
        language: "auto".to_string(),
        api_key: None,
    };

    let start = std::time::Instant::now();

    match pool.tts(tts_req).await {
        Ok(result) => {
            let runtime_ms = start.elapsed().as_millis() as i32;
            let filename = format!("{}.wav", job.id);
            let content_type = "audio/wav";

            match storage.upload(&filename, &result.audio_data, content_type, backend).await {
                Ok(upload) => {
                    sqlx::query(
                        "UPDATE jobs SET status = 'completed', audio_url = $1, duration_seconds = $2, actual_runtime_ms = $3, storage_type = $4, ipfs_cid = $5, pinning_cost = $6, completed_at = NOW() WHERE id = $7"
                    )
                    .bind(&upload.url)
                    .bind(result.duration_seconds)
                    .bind(runtime_ms)
                    .bind(&upload.storage_type)
                    .bind(&upload.ipfs_cid)
                    .bind(upload.pinning_cost)
                    .bind(&job.id)
                    .execute(&state.db)
                    .await?;

                    info!(
                        "job {} completed: {:.1}s audio, {}ms runtime",
                        job.id, result.duration_seconds, runtime_ms
                    );
                }
                Err(e) => {
                    error!("upload failed for job {}: {:?}", job.id, e);
                    mark_failed(&state.db, &job.id, "Upload failed").await;
                }
            }
        }
        Err(e) => {
            error!("TTS failed for job {}: {}", job.id, e);
            mark_failed(&state.db, &job.id, &format!("TTS: {}", e)).await;
        }
    }

    Ok(true)
}

async fn mark_failed(db: &sqlx::PgPool, job_id: &str, reason: &str) {
    let _ = sqlx::query("UPDATE jobs SET status = 'failed', error_message = $1, completed_at = NOW() WHERE id = $2")
        .bind(reason)
        .bind(job_id)
        .execute(db)
        .await;
}
