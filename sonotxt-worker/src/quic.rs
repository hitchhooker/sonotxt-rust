//! QUIC server — the worker's only external interface.
//!
//! The API connects here over QUIC, establishes a Noise_NK session,
//! and sends encrypted TTS/ASR requests. Results come back encrypted
//! over the same channel. No DB, no Redis — pure service.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::RwLock;
use tracing::{error, info, warn};

use sonotxt_core::noise::NoiseServer;
use sonotxt_core::protocol::{
    AttestationBundle, EncryptedAsrRequest, EncryptedAsrResponse,
    EncryptedTtsRequest, Message, TeeType, WorkerHealth,
};
use sonotxt_core::quic::{read_message, write_message};

use crate::processor::WorkerState;

pub struct QuicWorkerServer {
    noise: Arc<RwLock<NoiseServer>>,
    state: Arc<WorkerState>,
    start_time: Instant,
}

impl QuicWorkerServer {
    pub fn new(state: Arc<WorkerState>) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let noise = NoiseServer::new()?;
        Ok(Self {
            noise: Arc::new(RwLock::new(noise)),
            state,
            start_time: Instant::now(),
        })
    }

    pub async fn run(self, addr: SocketAddr) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let endpoint = sonotxt_core::quic::server_endpoint(addr)?;
        info!("QUIC server listening on {}", addr);

        while let Some(incoming) = endpoint.accept().await {
            let noise = self.noise.clone();
            let state = self.state.clone();
            let start_time = self.start_time;

            tokio::spawn(async move {
                match incoming.await {
                    Ok(conn) => {
                        info!("QUIC connection from {}", conn.remote_address());
                        if let Err(e) = handle_connection(conn, noise, state, start_time).await {
                            error!("QUIC connection error: {:?}", e);
                        }
                    }
                    Err(e) => error!("QUIC incoming failed: {:?}", e),
                }
            });
        }

        Ok(())
    }
}

async fn handle_connection(
    conn: quinn::Connection,
    noise: Arc<RwLock<NoiseServer>>,
    state: Arc<WorkerState>,
    start_time: Instant,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let session_id: Arc<RwLock<Option<[u8; 16]>>> = Arc::new(RwLock::new(None));

    loop {
        match conn.accept_bi().await {
            Ok((mut send, mut recv)) => {
                let noise = noise.clone();
                let state = state.clone();
                let sid = session_id.clone();
                let start = start_time;

                tokio::spawn(async move {
                    if let Err(e) = handle_stream(&mut send, &mut recv, noise, state, sid, start).await {
                        error!("QUIC stream error: {:?}", e);
                    }
                });
            }
            Err(quinn::ConnectionError::ApplicationClosed(_)) => {
                info!("QUIC connection closed");
                break;
            }
            Err(e) => {
                error!("QUIC accept error: {:?}", e);
                break;
            }
        }
    }

    if let Some(sid) = *session_id.read().await {
        noise.write().await.remove_session(&sid);
    }

    Ok(())
}

async fn handle_stream(
    send: &mut quinn::SendStream,
    recv: &mut quinn::RecvStream,
    noise: Arc<RwLock<NoiseServer>>,
    state: Arc<WorkerState>,
    session_id: Arc<RwLock<Option<[u8; 16]>>>,
    start_time: Instant,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let msg = read_message(recv).await?;

    match msg {
        Message::AttestationRequest => {
            let static_key = noise.read().await.static_public_key().to_vec();
            let attestation = generate_attestation(&static_key);
            write_message(send, &Message::Attestation(attestation)).await?;
        }

        Message::NoiseHandshake(client_msg) => {
            let (sid, response_msg) = noise.write().await.process_handshake(&client_msg)?;
            *session_id.write().await = Some(sid);
            info!("Noise session established");
            write_message(
                send,
                &Message::NoiseHandshakeResponse {
                    handshake: response_msg,
                    session_id: sid.to_vec(),
                },
            )
            .await?;
        }

        Message::EncryptedRequest(ciphertext) => {
            let sid = session_id.read().await.ok_or("no session")?;
            let plaintext = noise.write().await.decrypt(&sid, &ciphertext)?;
            let request: EncryptedTtsRequest = serde_json::from_slice(&plaintext)?;

            info!("TTS: voice={}, len={}", request.voice, request.text.len());

            let response = crate::processor::run_tts(&state, &request).await;

            let response_bytes = serde_json::to_vec(&response)?;
            let encrypted = noise.write().await.encrypt(&sid, &response_bytes)?;
            write_message(send, &Message::EncryptedResponse(encrypted)).await?;
        }

        Message::EncryptedAsrRequest(ciphertext) => {
            let sid = session_id.read().await.ok_or("no session")?;
            let plaintext = noise.write().await.decrypt(&sid, &ciphertext)?;
            let request: EncryptedAsrRequest = serde_json::from_slice(&plaintext)?;

            info!("ASR: audio_len={}", request.audio_base64.len());

            let response = match crate::processor::run_asr(&state, &request.audio_base64).await {
                Ok(text) => EncryptedAsrResponse {
                    request_id: request.request_id,
                    text,
                    error: None,
                },
                Err(e) => EncryptedAsrResponse {
                    request_id: request.request_id,
                    text: String::new(),
                    error: Some(e),
                },
            };

            let response_bytes = serde_json::to_vec(&response)?;
            let encrypted = noise.write().await.encrypt(&sid, &response_bytes)?;
            write_message(send, &Message::EncryptedAsrResponse(encrypted)).await?;
        }

        Message::HealthRequest => {
            let speech_ok = check_local(&state.http, &state.config.speech_url).await;
            let llm_ok = check_local(&state.http, &state.config.llm_url).await;

            write_message(
                send,
                &Message::HealthResponse(WorkerHealth {
                    speech_ok,
                    llm_ok,
                    jobs_processing: 0,
                    uptime_secs: start_time.elapsed().as_secs(),
                }),
            )
            .await?;
        }

        _ => {
            warn!("unexpected QUIC message type");
        }
    }

    send.finish()?;
    Ok(())
}

fn generate_attestation(static_key: &[u8]) -> AttestationBundle {
    use sha2::{Digest, Sha256};

    let mut quote_hasher = Sha256::new();
    quote_hasher.update(b"insecure-quote");
    let quote = quote_hasher.finalize().to_vec();

    let mut sig_hasher = Sha256::new();
    sig_hasher.update(&quote);
    sig_hasher.update(static_key);
    let binding = sig_hasher.finalize().to_vec();

    AttestationBundle {
        quote,
        static_key: static_key.to_vec(),
        binding_sig: binding,
        tee_type: TeeType::Insecure,
    }
}

async fn check_local(http: &reqwest::Client, base_url: &str) -> bool {
    match http
        .get(format!("{}/health", base_url))
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
    {
        Ok(r) => r.status().is_success(),
        Err(_) => false,
    }
}
