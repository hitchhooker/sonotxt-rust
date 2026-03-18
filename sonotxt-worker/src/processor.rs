use sonotxt_core::protocol::{EncryptedTtsRequest, EncryptedTtsResponse};
use tracing::{error, info};

use crate::config::WorkerConfig;

pub struct WorkerState {
    pub config: WorkerConfig,
    pub http: reqwest::Client,
}

/// Run TTS on local python service. Text exists only in memory.
pub async fn run_tts(state: &WorkerState, request: &EncryptedTtsRequest) -> EncryptedTtsResponse {
    #[derive(serde::Serialize)]
    struct SpeechReq {
        text: String,
        speaker: String,
        language: String,
    }

    let start = std::time::Instant::now();

    let mut req = state
        .http
        .post(format!("{}/synthesize", state.config.speech_url))
        .header("Content-Type", "application/json");

    if let Some(ref api_key) = state.config.speech_api_key {
        req = req.header("Authorization", format!("Bearer {}", api_key));
    }

    let result = req
        .json(&SpeechReq {
            text: request.text.clone(),
            speaker: request.voice.clone(),
            language: request.language.clone(),
        })
        .timeout(std::time::Duration::from_secs(180))
        .send()
        .await;

    match result {
        Ok(response) if response.status().is_success() => {
            match response.bytes().await {
                Ok(wav_data) => {
                    let duration_seconds = parse_wav_duration(&wav_data);

                    info!(
                        "TTS completed: {:.1}s audio, {} bytes, {}ms",
                        duration_seconds,
                        wav_data.len(),
                        start.elapsed().as_millis()
                    );

                    EncryptedTtsResponse {
                        request_id: request.request_id,
                        audio: wav_data.to_vec(),
                        format: "wav".to_string(),
                        duration_seconds,
                        error: None,
                    }
                }
                Err(e) => err_response(request.request_id, format!("read body: {}", e)),
            }
        }
        Ok(response) => {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            error!("speech service error {}: {}", status, body);
            err_response(request.request_id, format!("speech {}: {}", status, body))
        }
        Err(e) => {
            error!("speech service unreachable: {}", e);
            err_response(request.request_id, format!("request failed: {}", e))
        }
    }
}

/// Run ASR on local python service.
pub async fn run_asr(state: &WorkerState, audio_base64: &str) -> Result<String, String> {
    #[derive(serde::Serialize)]
    struct AsrReq { audio_base64: String }
    #[derive(serde::Deserialize)]
    struct AsrResp { text: String }

    let response = state
        .http
        .post(format!("{}/transcribe_base64", state.config.speech_url))
        .json(&AsrReq { audio_base64: audio_base64.to_string() })
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| format!("asr request: {}", e))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(format!("asr {}: {}", status, body));
    }

    let resp: AsrResp = response.json().await.map_err(|e| format!("asr json: {}", e))?;
    Ok(resp.text)
}

fn parse_wav_duration(wav_data: &[u8]) -> f64 {
    if wav_data.len() > 44 {
        let sr = u32::from_le_bytes([wav_data[24], wav_data[25], wav_data[26], wav_data[27]]);
        let ds = u32::from_le_bytes([wav_data[40], wav_data[41], wav_data[42], wav_data[43]]);
        ds as f64 / (sr as f64 * 2.0)
    } else {
        0.0
    }
}

fn err_response(request_id: [u8; 16], error: String) -> EncryptedTtsResponse {
    EncryptedTtsResponse {
        request_id,
        audio: vec![],
        format: String::new(),
        duration_seconds: 0.0,
        error: Some(error),
    }
}
