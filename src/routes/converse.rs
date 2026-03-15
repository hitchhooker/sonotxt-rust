//! Voice conversation endpoint: audio in → ASR → LLM → TTS → audio out
//! Streams sentence-by-sentence for low perceived latency.

use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::{error, info};

use crate::AppState;

#[derive(Debug, Deserialize)]
pub struct ConverseRequest {
    /// Base64-encoded audio (WAV/PCM)
    pub audio_base64: String,
    /// Conversation history (OpenAI-style messages)
    #[serde(default)]
    pub messages: Vec<ChatMessage>,
    /// TTS speaker voice
    #[serde(default = "default_speaker")]
    pub speaker: String,
    /// TTS language
    #[serde(default = "default_language")]
    pub language: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

fn default_speaker() -> String {
    "ryan".to_string()
}

fn default_language() -> String {
    "auto".to_string()
}

#[derive(Debug, Serialize)]
pub struct ConverseResponse {
    /// What the user said (ASR result)
    pub transcript: String,
    /// Full LLM response text
    pub response_text: String,
    /// Per-sentence TTS audio, base64-encoded WAV
    pub audio_segments: Vec<AudioSegment>,
    /// Timing breakdown
    pub timing: Timing,
}

#[derive(Debug, Serialize)]
pub struct AudioSegment {
    pub sentence: String,
    pub audio_base64: String,
    pub duration_seconds: f64,
}

#[derive(Debug, Serialize)]
pub struct Timing {
    pub asr_ms: u64,
    pub llm_ms: u64,
    pub tts_ms: Vec<u64>,
    pub total_ms: u64,
}

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/converse", post(converse))
        .route("/transcribe", post(transcribe))
        .route("/chat", post(chat))
        .route("/synthesize", post(synthesize))
}

/// Full voice pipeline: ASR → LLM → TTS (sentence-by-sentence)
async fn converse(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ConverseRequest>,
) -> Result<Json<ConverseResponse>, StatusCode> {
    let total_start = std::time::Instant::now();

    let speech_url = resolve_speech_url(&state)
        .ok_or_else(|| {
            error!("no speech worker available");
            StatusCode::SERVICE_UNAVAILABLE
        })?;
    let llm_url = resolve_llm_url(&state)
        .ok_or_else(|| {
            error!("no LLM worker available");
            StatusCode::SERVICE_UNAVAILABLE
        })?;

    // 1. ASR: audio → text
    let asr_start = std::time::Instant::now();
    let transcript = transcribe_audio(&state.http, &speech_url, &req.audio_base64).await
        .map_err(|e| {
            error!("ASR failed: {}", e);
            StatusCode::BAD_GATEWAY
        })?;
    let asr_ms = asr_start.elapsed().as_millis() as u64;
    info!("ASR: \"{}\" ({}ms)", transcript, asr_ms);

    // 2. LLM: build conversation and get sentence-split response
    let llm_start = std::time::Instant::now();
    let mut messages = req.messages.clone();
    messages.push(ChatMessage {
        role: "user".to_string(),
        content: transcript.clone(),
    });
    let (sentences, full_response) = chat_sentences(&state.http, &llm_url, &messages).await
        .map_err(|e| {
            error!("LLM failed: {}", e);
            StatusCode::BAD_GATEWAY
        })?;
    let llm_ms = llm_start.elapsed().as_millis() as u64;
    info!("LLM: {} sentences ({}ms)", sentences.len(), llm_ms);

    // 3. TTS: synthesize each sentence
    let mut audio_segments = Vec::new();
    let mut tts_times = Vec::new();

    for sentence in &sentences {
        let tts_start = std::time::Instant::now();
        match synthesize_sentence(&state.http, &speech_url, sentence, &req.speaker, &req.language).await {
            Ok((audio_b64, duration)) => {
                let tts_ms = tts_start.elapsed().as_millis() as u64;
                tts_times.push(tts_ms);
                audio_segments.push(AudioSegment {
                    sentence: sentence.clone(),
                    audio_base64: audio_b64,
                    duration_seconds: duration,
                });
            }
            Err(e) => {
                error!("TTS failed for sentence \"{}\": {}", sentence, e);
                tts_times.push(tts_start.elapsed().as_millis() as u64);
            }
        }
    }

    let total_ms = total_start.elapsed().as_millis() as u64;
    info!("Converse complete: {}ms total (ASR {}ms + LLM {}ms + TTS {:?}ms)",
        total_ms, asr_ms, llm_ms, tts_times);

    Ok(Json(ConverseResponse {
        transcript,
        response_text: full_response,
        audio_segments,
        timing: Timing {
            asr_ms,
            llm_ms,
            tts_ms: tts_times,
            total_ms,
        },
    }))
}

/// Standalone ASR endpoint
async fn transcribe(
    State(state): State<Arc<AppState>>,
    Json(req): Json<TranscribeRequest>,
) -> Result<Json<TranscribeResponse>, StatusCode> {
    let speech_url = resolve_speech_url(&state)
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;

    let transcript = transcribe_audio(&state.http, &speech_url, &req.audio_base64).await
        .map_err(|e| {
            error!("ASR failed: {}", e);
            StatusCode::BAD_GATEWAY
        })?;

    Ok(Json(TranscribeResponse { transcript }))
}

#[derive(Debug, Deserialize)]
struct TranscribeRequest {
    audio_base64: String,
}

#[derive(Debug, Serialize)]
struct TranscribeResponse {
    transcript: String,
}

/// Standalone LLM chat endpoint (proxies to Qwen3.5)
async fn chat(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ChatRequest>,
) -> Result<Json<ChatResponse>, StatusCode> {
    let llm_url = resolve_llm_url(&state)
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;

    let (sentences, full_response) = chat_sentences(&state.http, &llm_url, &req.messages).await
        .map_err(|e| {
            error!("LLM failed: {}", e);
            StatusCode::BAD_GATEWAY
        })?;

    Ok(Json(ChatResponse {
        response: full_response,
        sentences,
    }))
}

#[derive(Debug, Deserialize)]
struct ChatRequest {
    messages: Vec<ChatMessage>,
}

#[derive(Debug, Serialize)]
struct ChatResponse {
    response: String,
    sentences: Vec<String>,
}

/// Standalone TTS endpoint — proxies to Qwen speech service
async fn synthesize(
    State(state): State<Arc<AppState>>,
    Json(req): Json<SynthesizeRequest>,
) -> Result<Response, StatusCode> {
    let speech_url = resolve_speech_url(&state)
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;

    let (audio_b64, duration) = synthesize_sentence(
        &state.http, &speech_url, &req.text, &req.speaker, &req.language,
    ).await.map_err(|e| {
        error!("TTS failed: {}", e);
        StatusCode::BAD_GATEWAY
    })?;

    Ok(Json(SynthesizeResponse { audio_base64: audio_b64, duration_seconds: duration }).into_response())
}

#[derive(Debug, Deserialize)]
struct SynthesizeRequest {
    text: String,
    #[serde(default = "default_speaker")]
    speaker: String,
    #[serde(default = "default_language")]
    language: String,
}

#[derive(Debug, Serialize)]
struct SynthesizeResponse {
    audio_base64: String,
    duration_seconds: f64,
}

// ── worker resolution ────────────────────────────────────────────────

/// Resolve speech URL from worker pool or config fallback.
fn resolve_speech_url(state: &AppState) -> Option<String> {
    if let Some(ref pool) = state.workers {
        pool.pick().map(|w| w.speech_url.clone())
    } else {
        state.config.qwen_speech_url.clone()
    }
}

/// Resolve LLM URL from worker pool or config fallback.
fn resolve_llm_url(state: &AppState) -> Option<String> {
    if let Some(ref pool) = state.workers {
        pool.pick_llm().map(|w| w.llm_url.clone())
    } else {
        state.config.qwen_llm_url.clone()
    }
}

// ── internal helpers ──────────────────────────────────────────────────

async fn transcribe_audio(
    http: &reqwest::Client,
    speech_url: &str,
    audio_base64: &str,
) -> anyhow::Result<String> {
    #[derive(Serialize)]
    struct Req {
        audio_base64: String,
    }
    #[derive(Deserialize)]
    struct Resp {
        text: String,
    }

    let resp: Resp = http
        .post(format!("{}/transcribe_base64", speech_url))
        .json(&Req { audio_base64: audio_base64.to_string() })
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    Ok(resp.text)
}

async fn chat_sentences(
    http: &reqwest::Client,
    llm_url: &str,
    messages: &[ChatMessage],
) -> anyhow::Result<(Vec<String>, String)> {
    #[derive(Serialize)]
    struct Req {
        messages: Vec<ChatMessage>,
    }
    #[derive(Deserialize)]
    struct Resp {
        sentences: Vec<String>,
        full_response: String,
    }

    let resp: Resp = http
        .post(format!("{}/chat_sentences", llm_url))
        .json(&Req { messages: messages.to_vec() })
        .timeout(std::time::Duration::from_secs(60))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    Ok((resp.sentences, resp.full_response))
}

async fn synthesize_sentence(
    http: &reqwest::Client,
    speech_url: &str,
    text: &str,
    speaker: &str,
    language: &str,
) -> anyhow::Result<(String, f64)> {
    #[derive(Serialize)]
    struct Req {
        text: String,
        speaker: String,
        language: String,
    }

    let resp = http
        .post(format!("{}/synthesize", speech_url))
        .json(&Req {
            text: text.to_string(),
            speaker: speaker.to_string(),
            language: language.to_string(),
        })
        .timeout(std::time::Duration::from_secs(120))
        .send()
        .await?
        .error_for_status()?;

    // Response is raw WAV bytes
    let wav_bytes = resp.bytes().await?;

    // Parse duration from WAV header
    let duration = if wav_bytes.len() > 44 {
        let sample_rate = u32::from_le_bytes([wav_bytes[24], wav_bytes[25], wav_bytes[26], wav_bytes[27]]);
        let data_size = u32::from_le_bytes([wav_bytes[40], wav_bytes[41], wav_bytes[42], wav_bytes[43]]);
        data_size as f64 / (sample_rate as f64 * 2.0)
    } else {
        0.0
    };

    // Base64-encode for JSON response
    use base64::{engine::general_purpose::STANDARD, Engine};
    let audio_b64 = STANDARD.encode(&wav_bytes);

    Ok((audio_b64, duration))
}
