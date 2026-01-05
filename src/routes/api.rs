use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::header,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, sync::Arc};
use uuid::Uuid;

use crate::{
    auth::{AuthenticatedUser, TtsUser, check_free_tier_limit},
    error::Result,
    models::{JobStatus, ProcessRequest, ProcessResponse},
    services::content::extract_content,
    AppState,
};

#[derive(Debug, Deserialize)]
struct TtsRequest {
    text: String,
    #[serde(default = "default_voice")]
    voice: String,
    #[serde(default)]
    storage: Option<String>, // "minio" or "ipfs"
}

fn default_voice() -> String {
    "af_bella".to_string()
}

#[derive(Debug, Serialize)]
struct TtsResponse {
    job_id: String,
    status: JobStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    estimated_cost: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    free_tier_remaining: Option<i32>,
}

const VALID_VOICES: &[&str] = &[
    "af_alloy", "af_aoede", "af_bella", "af_heart", "af_jessica", "af_kore",
    "af_nicole", "af_nova", "af_river", "af_sarah", "af_sky",
    "am_adam", "am_echo", "am_eric", "am_fenrir", "am_liam", "am_michael",
    "am_onyx", "am_puck", "am_santa",
    "bf_alice", "bf_emma", "bf_isabella", "bf_lily",
    "bm_daniel", "bm_fable", "bm_george", "bm_lewis",
    "ef_dora", "em_alex", "em_santa", "ff_siwis",
    "hf_alpha", "hf_beta", "hm_omega", "hm_psi",
    "if_sara", "im_nicola",
    "jf_alpha", "jf_gongitsune", "jf_nezumi", "jf_tebukuro", "jm_kumo",
    "pf_dora", "pm_alex", "pm_santa",
    "zf_xiaobei", "zf_xiaoni", "zf_xiaoxiao", "zf_xiaoyi",
    "zm_yunjian", "zm_yunxi", "zm_yunxia", "zm_yunyang",
];

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/process", post(process))
        .route("/tts", post(tts))
        .route("/extract", post(extract))
        .route("/status", get(status))
        .route("/voices", get(list_voices))
        .route("/download/{job_id}", get(download_audio))
}

async fn list_voices(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let samples_url = format!("{}/samples", state.config.audio_public_url);

    // Build voices with sample URLs
    let voices_with_samples: Vec<serde_json::Value> = VALID_VOICES
        .iter()
        .map(|v| {
            serde_json::json!({
                "id": v,
                "sample_url": format!("{}/{}.mp3", samples_url, v)
            })
        })
        .collect();

    Json(serde_json::json!({
        "voices": voices_with_samples,
        "default": "af_bella",
        "samples_base_url": samples_url,
        "categories": {
            "american_female": ["af_alloy", "af_aoede", "af_bella", "af_heart", "af_jessica", "af_kore", "af_nicole", "af_nova", "af_river", "af_sarah", "af_sky"],
            "american_male": ["am_adam", "am_echo", "am_eric", "am_fenrir", "am_liam", "am_michael", "am_onyx", "am_puck", "am_santa"],
            "british_female": ["bf_alice", "bf_emma", "bf_isabella", "bf_lily"],
            "british_male": ["bm_daniel", "bm_fable", "bm_george", "bm_lewis"],
            "japanese": ["jf_alpha", "jf_gongitsune", "jf_nezumi", "jf_tebukuro", "jm_kumo"],
            "chinese": ["zf_xiaobei", "zf_xiaoni", "zf_xiaoxiao", "zf_xiaoyi", "zm_yunjian", "zm_yunxi", "zm_yunxia", "zm_yunyang"]
        }
    }))
}

#[derive(Debug, Deserialize)]
struct ExtractRequest {
    url: String,
    #[serde(default)]
    selector: Option<String>,
}

#[derive(Debug, Serialize)]
struct ExtractResponse {
    text: String,
    title: Option<String>,
    char_count: usize,
    word_count: usize,
}

async fn extract(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ExtractRequest>,
) -> Result<Json<ExtractResponse>> {
    let content = extract_content(&state, &req.url, req.selector.as_deref()).await?;
    let word_count = content.text.split_whitespace().count();

    Ok(Json(ExtractResponse {
        char_count: content.text.len(),
        word_count,
        title: content.title,
        text: content.text,
    }))
}

async fn process(
    State(state): State<Arc<AppState>>,
    user: AuthenticatedUser,
    Json(req): Json<ProcessRequest>,
) -> Result<Json<ProcessResponse>> {
    let extracted = extract_content(&state, &req.url, req.selector.as_deref()).await?;
    let content = extracted.text;
    let estimated_cost = (content.len() as f64) * state.config.cost_per_char;

    // Check balance from database
    let balance = sqlx::query_scalar!(
        "SELECT balance FROM account_credits WHERE account_id = $1",
        user.account_id
    )
    .fetch_optional(&state.db)
    .await?
    .unwrap_or(0.0);

    if balance < estimated_cost {
        return Err(crate::error::ApiError::InsufficientBalance);
    }

    let job_id = Uuid::new_v4().to_string();

    // Atomically reserve balance and create job
    let mut tx = state.db.begin().await?;

    let rows_affected = sqlx::query!(
        "UPDATE account_credits SET balance = balance - $1 WHERE account_id = $2 AND balance >= $1",
        estimated_cost,
        user.account_id
    )
    .execute(&mut *tx)
    .await?
    .rows_affected();

    if rows_affected == 0 {
        return Err(crate::error::ApiError::InsufficientBalance);
    }

    sqlx::query!(
        "INSERT INTO jobs (id, api_key, text_content, status, cost) VALUES ($1, $2, $3, 'queued', $4)",
        job_id,
        user.api_key,
        content.as_str(),
        estimated_cost
    )
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    Ok(Json(ProcessResponse {
        job_id,
        status: JobStatus::Queued { position: None, estimated_seconds: None },
        estimated_cost,
    }))
}

async fn tts(
    State(state): State<Arc<AppState>>,
    user: TtsUser,
    Json(req): Json<TtsRequest>,
) -> Result<Json<TtsResponse>> {
    let text = req.text.trim();

    if text.is_empty() {
        return Err(crate::error::ApiError::InvalidRequestError);
    }

    // free tier gets smaller limit
    let max_size = if user.is_free_tier() {
        1000
    } else {
        state.config.max_content_size
    };

    if text.len() > max_size {
        return Err(crate::error::ApiError::ContentTooLarge);
    }

    // validate voice
    let voice = if VALID_VOICES.contains(&req.voice.as_str()) {
        req.voice.clone()
    } else {
        default_voice()
    };

    let job_id = Uuid::new_v4().to_string();
    let char_count = text.len() as i32;

    match user {
        TtsUser::Authenticated(auth_user) => {
            let estimated_cost = (text.len() as f64) * state.config.cost_per_char;

            let balance = sqlx::query_scalar!(
                "SELECT balance FROM account_credits WHERE account_id = $1",
                auth_user.account_id
            )
            .fetch_optional(&state.db)
            .await?
            .unwrap_or(0.0);

            if balance < estimated_cost {
                return Err(crate::error::ApiError::InsufficientBalance);
            }

            let mut tx = state.db.begin().await?;

            let rows_affected = sqlx::query!(
                "UPDATE account_credits SET balance = balance - $1 WHERE account_id = $2 AND balance >= $1",
                estimated_cost,
                auth_user.account_id
            )
            .execute(&mut *tx)
            .await?
            .rows_affected();

            if rows_affected == 0 {
                return Err(crate::error::ApiError::InsufficientBalance);
            }

            let estimated_duration_ms = (char_count as f64 * crate::models::MS_PER_CHAR) as i32;
            let storage_type = req.storage.as_deref();

            sqlx::query!(
                "INSERT INTO jobs (id, api_key, text_content, voice, status, cost, is_free_tier, char_count, estimated_duration_ms, storage_type) VALUES ($1, $2, $3, $4, 'queued', $5, FALSE, $6, $7, $8)",
                job_id,
                auth_user.api_key,
                text,
                voice,
                estimated_cost,
                char_count,
                estimated_duration_ms,
                storage_type
            )
            .execute(&mut *tx)
            .await?;

            tx.commit().await?;

            let estimated_seconds = estimated_duration_ms as f64 / 1000.0;
            Ok(Json(TtsResponse {
                job_id,
                status: JobStatus::Queued { position: None, estimated_seconds: Some(estimated_seconds) },
                estimated_cost: Some(estimated_cost),
                free_tier_remaining: None,
            }))
        }

        TtsUser::FreeTier { ip_hash } => {
            // check and consume free tier allowance
            let remaining = check_free_tier_limit(&state.db, &ip_hash, char_count).await?;

            let mut tx = state.db.begin().await?;

            // consume the chars
            sqlx::query!(
                "UPDATE free_tier_usage SET chars_used = chars_used + $1 WHERE ip_hash = $2",
                char_count,
                ip_hash
            )
            .execute(&mut *tx)
            .await?;

            let estimated_duration_ms = (char_count as f64 * crate::models::MS_PER_CHAR) as i32;
            // free tier only gets minio, not ipfs (to avoid pinning costs)
            let storage_type: Option<&str> = Some("minio");

            // create job with ip_hash instead of api_key
            sqlx::query!(
                "INSERT INTO jobs (id, ip_hash, text_content, voice, status, cost, is_free_tier, char_count, estimated_duration_ms, storage_type) VALUES ($1, $2, $3, $4, 'queued', 0, TRUE, $5, $6, $7)",
                job_id,
                ip_hash,
                text,
                voice,
                char_count,
                estimated_duration_ms,
                storage_type
            )
            .execute(&mut *tx)
            .await?;

            tx.commit().await?;

            let estimated_seconds = estimated_duration_ms as f64 / 1000.0;
            Ok(Json(TtsResponse {
                job_id,
                status: JobStatus::Queued { position: None, estimated_seconds: Some(estimated_seconds) },
                estimated_cost: None,
                free_tier_remaining: Some(remaining - char_count),
            }))
        }
    }
}

async fn status(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<JobStatus>> {
    let job_id = params
        .get("job_id")
        .ok_or(crate::error::ApiError::InvalidRequestError)?;

    let job = sqlx::query!(
        r#"SELECT
            status,
            audio_url,
            duration_seconds,
            error_message,
            estimated_duration_ms,
            actual_runtime_ms,
            deepinfra_cost,
            started_at,
            created_at,
            storage_type,
            ipfs_cid
        FROM jobs WHERE id = $1"#,
        job_id
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(crate::error::ApiError::NotFound)?;

    let estimated_seconds = job.estimated_duration_ms.map(|ms| ms as f64 / 1000.0);

    match job.status.as_str() {
        "completed" => Ok(Json(JobStatus::Complete {
            url: job.audio_url.unwrap_or_default(),
            duration_seconds: job.duration_seconds.unwrap_or(0.0),
            runtime_ms: job.actual_runtime_ms,
            cost: job.deepinfra_cost,
            storage_type: job.storage_type,
            ipfs_cid: job.ipfs_cid,
        })),
        "failed" => Ok(Json(JobStatus::Failed {
            reason: job.error_message.unwrap_or_else(|| "Processing failed".into()),
        })),
        "processing" => {
            // Calculate progress based on elapsed time vs estimated
            let elapsed_seconds = job.started_at
                .map(|started| {
                    let now = chrono::Utc::now();
                    (now - started).num_milliseconds() as f64 / 1000.0
                });

            let progress: u8 = match (elapsed_seconds, estimated_seconds) {
                (Some(elapsed), Some(estimated)) if estimated > 0.0 => {
                    let pct = ((elapsed / estimated) * 100.0).min(99.0) as u8;
                    pct.max(1) // at least 1%
                }
                _ => 50, // fallback
            };

            Ok(Json(JobStatus::Processing {
                progress,
                elapsed_seconds,
                estimated_seconds,
            }))
        }
        "queued" => {
            // Count position in queue
            let position = sqlx::query_scalar!(
                "SELECT COUNT(*) FROM jobs WHERE status = 'queued' AND created_at < $1",
                job.created_at
            )
            .fetch_one(&state.db)
            .await?
            .map(|c| c as u32);

            Ok(Json(JobStatus::Queued {
                position,
                estimated_seconds,
            }))
        }
        _ => Ok(Json(JobStatus::Queued { position: None, estimated_seconds })),
    }
}

async fn download_audio(
    State(state): State<Arc<AppState>>,
    Path(job_id): Path<String>,
) -> Result<impl IntoResponse> {
    // get audio url from job - audio_url is nullable TEXT so we get Option<Option<String>>
    let audio_url: String = sqlx::query_scalar::<_, Option<String>>(
        "SELECT audio_url FROM jobs WHERE id = $1 AND status = 'completed'"
    )
    .bind(&job_id)
    .fetch_optional(&state.db)
    .await?
    .flatten() // Option<Option<String>> -> Option<String>
    .ok_or(crate::error::ApiError::NotFound)?;

    // fetch audio from storage
    let client = reqwest::Client::new();
    let response = client
        .get(&audio_url)
        .send()
        .await
        .map_err(|_| crate::error::ApiError::InternalError)?;

    if !response.status().is_success() {
        return Err(crate::error::ApiError::NotFound);
    }

    let bytes = response
        .bytes()
        .await
        .map_err(|_| crate::error::ApiError::InternalError)?;

    // detect format from url extension
    let (extension, content_type) = if audio_url.ends_with(".ogg") {
        ("ogg", "audio/ogg")
    } else {
        ("mp3", "audio/mpeg")
    };

    let filename = format!("sonotxt-{}.{}", job_id, extension);

    Ok(Response::builder()
        .header(header::CONTENT_TYPE, content_type)
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{}\"", filename),
        )
        .body(Body::from(bytes))
        .unwrap())
}
