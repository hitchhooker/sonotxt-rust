//! Voice conversation endpoint: audio in → ASR → LLM → TTS → audio out
//!
//! All GPU communication flows through the WorkerPool service layer.
//! No raw HTTP here — just Service calls with load balancing,
//! timeouts, retries, and health checks composed in.
//!
//! Pipeline: ASR(audio) flatMap { text =>
//!             LLM(text) flatMap { sentences =>
//!               collect(sentences.map(TTS)) }}

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
use crate::services::worker_pool::{
    AsrRequest, LlmRequest, LlmMessage, TtsRequest, ServiceError,
};

#[derive(Debug, Deserialize)]
pub struct ConverseRequest {
    pub audio_base64: String,
    #[serde(default)]
    pub messages: Vec<ChatMessage>,
    #[serde(default = "default_speaker")]
    pub speaker: String,
    #[serde(default = "default_language")]
    pub language: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

fn default_speaker() -> String { "ryan".to_string() }
fn default_language() -> String { "auto".to_string() }

#[derive(Debug, Serialize)]
pub struct ConverseResponse {
    pub transcript: String,
    pub response_text: String,
    pub audio_segments: Vec<AudioSegment>,
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

fn svc_err(e: ServiceError) -> StatusCode {
    match e {
        ServiceError::Timeout => StatusCode::GATEWAY_TIMEOUT,
        ServiceError::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
        _ => StatusCode::BAD_GATEWAY,
    }
}

/// Full voice pipeline: ASR → LLM → TTS (sentence-by-sentence)
///
/// In Eriksen's terms:
///   asr(audio) flatMap { transcript =>
///     llm(messages ++ transcript) flatMap { sentences =>
///       collect(sentences.map(tts))
///     }
///   }
async fn converse(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ConverseRequest>,
) -> Result<Json<ConverseResponse>, StatusCode> {
    let pool = state.workers.as_ref().ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let total_start = std::time::Instant::now();

    // 1. ASR: audio → text
    let asr_start = std::time::Instant::now();
    let asr_resp = pool.asr(AsrRequest { audio_base64: req.audio_base64 }).await
        .map_err(|e| { error!("ASR: {}", e); svc_err(e) })?;
    let asr_ms = asr_start.elapsed().as_millis() as u64;
    info!("ASR: \"{}\" ({}ms)", asr_resp.text, asr_ms);

    // 2. LLM: conversation → sentences
    let llm_start = std::time::Instant::now();
    let mut messages: Vec<LlmMessage> = req.messages.iter()
        .map(|m| LlmMessage { role: m.role.clone(), content: m.content.clone() })
        .collect();
    messages.push(LlmMessage { role: "user".to_string(), content: asr_resp.text.clone() });

    let llm_resp = pool.llm(LlmRequest {
        messages,
        max_tokens: 512,
        temperature: 1.0,
    }).await.map_err(|e| { error!("LLM: {}", e); svc_err(e) })?;
    let llm_ms = llm_start.elapsed().as_millis() as u64;
    info!("LLM: {} sentences ({}ms)", llm_resp.sentences.len(), llm_ms);

    // 3. TTS: collect(sentences.map(tts))
    let mut audio_segments = Vec::new();
    let mut tts_times = Vec::new();

    for sentence in &llm_resp.sentences {
        let tts_start = std::time::Instant::now();
        match pool.tts(TtsRequest {
            text: sentence.clone(),
            speaker: req.speaker.clone(),
            language: req.language.clone(),
            api_key: state.config.qwen_speech_api_key.clone(),
        }).await {
            Ok(resp) => {
                use base64::{engine::general_purpose::STANDARD, Engine};
                tts_times.push(tts_start.elapsed().as_millis() as u64);
                audio_segments.push(AudioSegment {
                    sentence: sentence.clone(),
                    audio_base64: STANDARD.encode(&resp.audio_data),
                    duration_seconds: resp.duration_seconds,
                });
            }
            Err(e) => {
                error!("TTS \"{}\": {}", sentence, e);
                tts_times.push(tts_start.elapsed().as_millis() as u64);
            }
        }
    }

    let total_ms = total_start.elapsed().as_millis() as u64;
    info!("converse: {}ms (asr={}ms llm={}ms tts={:?}ms)", total_ms, asr_ms, llm_ms, tts_times);

    Ok(Json(ConverseResponse {
        transcript: asr_resp.text,
        response_text: llm_resp.full_response,
        audio_segments,
        timing: Timing { asr_ms, llm_ms, tts_ms: tts_times, total_ms },
    }))
}

/// Standalone ASR: pool.asr(audio)
async fn transcribe(
    State(state): State<Arc<AppState>>,
    Json(req): Json<TranscribeRequest>,
) -> Result<Json<TranscribeResponse>, StatusCode> {
    let pool = state.workers.as_ref().ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let resp = pool.asr(AsrRequest { audio_base64: req.audio_base64 }).await
        .map_err(|e| { error!("ASR: {}", e); svc_err(e) })?;
    Ok(Json(TranscribeResponse { transcript: resp.text }))
}

#[derive(Debug, Deserialize)]
struct TranscribeRequest { audio_base64: String }

#[derive(Debug, Serialize)]
struct TranscribeResponse { transcript: String }

/// Standalone LLM: pool.llm(messages)
async fn chat(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ChatRequest>,
) -> Result<Json<ChatResponse>, StatusCode> {
    let pool = state.workers.as_ref().ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let messages = req.messages.iter()
        .map(|m| LlmMessage { role: m.role.clone(), content: m.content.clone() })
        .collect();
    let resp = pool.llm(LlmRequest { messages, max_tokens: 512, temperature: 1.0 }).await
        .map_err(|e| { error!("LLM: {}", e); svc_err(e) })?;
    Ok(Json(ChatResponse { response: resp.full_response, sentences: resp.sentences }))
}

#[derive(Debug, Deserialize)]
struct ChatRequest { messages: Vec<ChatMessage> }

#[derive(Debug, Serialize)]
struct ChatResponse { response: String, sentences: Vec<String> }

/// Standalone TTS: pool.tts(text)
async fn synthesize(
    State(state): State<Arc<AppState>>,
    Json(req): Json<SynthesizeRequest>,
) -> Result<Response, StatusCode> {
    let pool = state.workers.as_ref().ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let resp = pool.tts(TtsRequest {
        text: req.text,
        speaker: req.speaker,
        language: req.language,
        api_key: state.config.qwen_speech_api_key.clone(),
    }).await.map_err(|e| { error!("TTS: {}", e); svc_err(e) })?;

    use base64::{engine::general_purpose::STANDARD, Engine};
    Ok(Json(SynthesizeResponse {
        audio_base64: STANDARD.encode(&resp.audio_data),
        duration_seconds: resp.duration_seconds,
    }).into_response())
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
