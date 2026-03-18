//! QUIC-based worker connections for the API.
//!
//! Each QuicWorkerConn maintains a persistent QUIC connection to a worker,
//! with a Noise_NK encrypted session. Used by WorkerPool as an alternative
//! to HTTP when QUIC URLs are configured.
//!
//! Provides:
//! - Job push notifications (replaces redis pub/sub)
//! - Encrypted TTS inference (text never leaves encrypted channel)
//! - Low-latency health checks over QUIC

use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{error, info, warn};

use sonotxt_core::noise::NoiseClient;
use sonotxt_core::protocol::{
    AttestationBundle, EncryptedTtsRequest, EncryptedTtsResponse, Message, TeeType, WorkerHealth,
};
use sonotxt_core::quic::{read_message, write_message};

/// A persistent QUIC connection to one worker with Noise encryption.
pub struct QuicWorkerConn {
    endpoint: quinn::Endpoint,
    connection: quinn::Connection,
    noise: RwLock<NoiseClient>,
    addr: SocketAddr,
}

impl QuicWorkerConn {
    /// Connect to a worker, verify attestation, establish Noise session.
    pub async fn connect(addr: SocketAddr) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let endpoint = sonotxt_core::quic::client_endpoint()?;

        info!("QUIC connecting to {}", addr);
        let connection = endpoint.connect(addr, "localhost")?.await?;
        info!("QUIC connected to {}", addr);

        // Request attestation
        let attestation = request_attestation(&connection).await?;
        info!("attestation received: {:?}", attestation.tee_type);

        verify_attestation(&attestation)?;
        info!("attestation verified");

        // Noise handshake
        let mut noise = NoiseClient::new();
        let handshake_msg = noise.initiate_handshake(&attestation.static_key)?;
        let (server_response, session_id) =
            send_noise_handshake(&connection, &handshake_msg).await?;
        noise.complete_handshake(&server_response, session_id)?;

        info!("Noise session established with {}", addr);

        Ok(Self {
            endpoint,
            connection,
            noise: RwLock::new(noise),
            addr,
        })
    }

    /// Push job notification to worker (encrypted).
    pub async fn notify_job(&self, job_id: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let ciphertext = self.noise.write().await.encrypt(job_id.as_bytes())?;

        let (mut send, mut recv) = self.connection.open_bi().await?;
        write_message(&mut send, &Message::JobNotify(ciphertext)).await?;
        send.finish()?;

        let response = read_message(&mut recv).await?;
        match response {
            Message::JobAck { job_id: ack_id } => {
                if ack_id != job_id {
                    warn!("job ack mismatch: expected {}, got {}", job_id, ack_id);
                }
                Ok(())
            }
            _ => Err("unexpected response to job notify".into()),
        }
    }

    /// Encrypted TTS: text is encrypted end-to-end, never hits disk on worker.
    pub async fn encrypted_tts(
        &self,
        request: &EncryptedTtsRequest,
    ) -> Result<EncryptedTtsResponse, Box<dyn std::error::Error + Send + Sync>> {
        let plaintext = serde_json::to_vec(request)?;
        let ciphertext = self.noise.write().await.encrypt(&plaintext)?;

        let (mut send, mut recv) = self.connection.open_bi().await?;
        write_message(&mut send, &Message::EncryptedRequest(ciphertext)).await?;
        send.finish()?;

        let response = read_message(&mut recv).await?;
        match response {
            Message::EncryptedResponse(encrypted) => {
                let decrypted = self.noise.write().await.decrypt(&encrypted)?;
                let tts_response: EncryptedTtsResponse = serde_json::from_slice(&decrypted)?;
                Ok(tts_response)
            }
            _ => Err("unexpected response type".into()),
        }
    }

    /// Encrypted ASR: audio encrypted end-to-end via Noise channel.
    pub async fn encrypted_asr(
        &self,
        audio_base64: &str,
    ) -> Result<sonotxt_core::EncryptedAsrResponse, Box<dyn std::error::Error + Send + Sync>> {
        let mut request_id = [0u8; 16];
        rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut request_id);

        let request = sonotxt_core::EncryptedAsrRequest {
            request_id,
            audio_base64: audio_base64.to_string(),
        };

        let plaintext = serde_json::to_vec(&request)?;
        let ciphertext = self.noise.write().await.encrypt(&plaintext)?;

        let (mut send, mut recv) = self.connection.open_bi().await?;
        write_message(&mut send, &Message::EncryptedAsrRequest(ciphertext)).await?;
        send.finish()?;

        let response = read_message(&mut recv).await?;
        match response {
            Message::EncryptedAsrResponse(encrypted) => {
                let decrypted = self.noise.write().await.decrypt(&encrypted)?;
                let asr_response: sonotxt_core::EncryptedAsrResponse = serde_json::from_slice(&decrypted)?;
                Ok(asr_response)
            }
            _ => Err("unexpected response type".into()),
        }
    }

    /// Health check over QUIC (faster than HTTP, no TLS handshake).
    pub async fn health(&self) -> Result<WorkerHealth, Box<dyn std::error::Error + Send + Sync>> {
        let (mut send, mut recv) = self.connection.open_bi().await?;
        write_message(&mut send, &Message::HealthRequest).await?;
        send.finish()?;

        let response = read_message(&mut recv).await?;
        match response {
            Message::HealthResponse(health) => Ok(health),
            _ => Err("unexpected response type".into()),
        }
    }

    pub fn remote_addr(&self) -> SocketAddr {
        self.addr
    }
}

async fn request_attestation(
    conn: &quinn::Connection,
) -> Result<AttestationBundle, Box<dyn std::error::Error + Send + Sync>> {
    let (mut send, mut recv) = conn.open_bi().await?;
    write_message(&mut send, &Message::AttestationRequest).await?;
    send.finish()?;

    let msg = read_message(&mut recv).await?;
    match msg {
        Message::Attestation(bundle) => Ok(bundle),
        _ => Err("expected attestation response".into()),
    }
}

fn verify_attestation(bundle: &AttestationBundle) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use sha2::{Digest, Sha256};

    // Verify binding: H(quote || static_key) == binding_sig
    let mut hasher = Sha256::new();
    hasher.update(&bundle.quote);
    hasher.update(&bundle.static_key);
    let expected = hasher.finalize();

    if bundle.binding_sig != expected.as_slice() {
        return Err("attestation binding signature mismatch".into());
    }

    match bundle.tee_type {
        TeeType::Insecure => {
            warn!("accepting insecure attestation (development mode)");
            Ok(())
        }
        TeeType::SevSnp => {
            // TODO: verify AMD SEV-SNP attestation chain
            Err("SEV-SNP verification not yet implemented".into())
        }
        TeeType::Tdx => {
            // TODO: verify Intel TDX attestation chain
            Err("TDX verification not yet implemented".into())
        }
    }
}

async fn send_noise_handshake(
    conn: &quinn::Connection,
    handshake_msg: &[u8],
) -> Result<(Vec<u8>, [u8; 16]), Box<dyn std::error::Error + Send + Sync>> {
    let (mut send, mut recv) = conn.open_bi().await?;
    write_message(&mut send, &Message::NoiseHandshake(handshake_msg.to_vec())).await?;
    send.finish()?;

    let msg = read_message(&mut recv).await?;
    match msg {
        Message::NoiseHandshakeResponse {
            handshake,
            session_id,
        } => {
            if session_id.len() != 16 {
                return Err("invalid session id length".into());
            }
            let mut sid = [0u8; 16];
            sid.copy_from_slice(&session_id);
            Ok((handshake, sid))
        }
        _ => Err("expected handshake response".into()),
    }
}
