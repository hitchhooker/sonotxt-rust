//! QUIC wire protocol for API ↔ worker communication.
//!
//! All messages are length-prefixed JSON. The outer QUIC stream provides
//! reliability; Noise_NK provides confidentiality and authentication.
//!
//! Flow:
//!   1. API connects via QUIC
//!   2. API requests attestation → worker sends static key + TEE quote
//!   3. Noise_NK handshake (API knows worker's static key from attestation)
//!   4. All subsequent messages encrypted with Noise transport
//!
//! Once the session is established, the API can:
//!   - Push job notifications (worker wakes and polls DB)
//!   - Request direct TTS inference (text stays encrypted end-to-end)
//!   - Poll health status

use serde::{Deserialize, Serialize};

/// TEE attestation bundle binding Noise static key to TEE identity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttestationBundle {
    /// Raw attestation quote (SEV-SNP, TDX, or insecure stub)
    pub quote: Vec<u8>,
    /// Noise static public key for this worker (32 bytes X25519)
    pub static_key: Vec<u8>,
    /// H(quote || static_key) — binds key to attestation
    pub binding_sig: Vec<u8>,
    /// TEE type for verification dispatch
    pub tee_type: TeeType,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub enum TeeType {
    SevSnp,
    Tdx,
    /// Development mode — no real attestation
    Insecure,
}

/// QUIC stream message envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Message {
    // ── Handshake ──────────────────────────────────
    /// Client requesting attestation
    AttestationRequest,
    /// Worker responding with attestation
    Attestation(AttestationBundle),
    /// Noise handshake: client → server (→ e, es)
    NoiseHandshake(Vec<u8>),
    /// Noise handshake response: server → client (← e, ee) + session ID
    NoiseHandshakeResponse {
        handshake: Vec<u8>,
        session_id: Vec<u8>,
    },

    // ── Job dispatch ──────────────────────────────
    /// API notifies worker of new job (encrypted payload: job_id)
    JobNotify(Vec<u8>),
    /// Worker acknowledges job pickup
    JobAck { job_id: String },

    // ── Encrypted inference ──────────────────────
    /// Encrypted TTS request (Noise ciphertext)
    EncryptedRequest(Vec<u8>),
    /// Encrypted TTS response (Noise ciphertext)
    EncryptedResponse(Vec<u8>),
    /// Encrypted ASR request (Noise ciphertext)
    EncryptedAsrRequest(Vec<u8>),
    /// Encrypted ASR response (Noise ciphertext)
    EncryptedAsrResponse(Vec<u8>),
    /// Encrypted streaming request
    EncryptedStreamRequest(Vec<u8>),
    /// Encrypted streaming audio chunk
    EncryptedStreamChunk(Vec<u8>),

    // ── Health ────────────────────────────────────
    HealthRequest,
    HealthResponse(WorkerHealth),
}

/// Worker health status sent over QUIC.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerHealth {
    pub speech_ok: bool,
    pub llm_ok: bool,
    pub jobs_processing: u32,
    pub uptime_secs: u64,
}

/// Direct TTS request (sent inside Noise channel, text never hits disk).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncryptedTtsRequest {
    pub request_id: [u8; 16],
    pub text: String,
    pub voice: String,
    pub speed: f32,
    pub language: String,
}

/// Direct TTS response (sent inside Noise channel).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncryptedTtsResponse {
    pub request_id: [u8; 16],
    pub audio: Vec<u8>,
    pub format: String,
    pub duration_seconds: f64,
    pub error: Option<String>,
}

/// ASR request (sent inside Noise channel).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncryptedAsrRequest {
    pub request_id: [u8; 16],
    pub audio_base64: String,
}

/// ASR response (sent inside Noise channel).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncryptedAsrResponse {
    pub request_id: [u8; 16],
    pub text: String,
    pub error: Option<String>,
}

/// Streaming audio chunk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamChunk {
    pub request_id: [u8; 16],
    pub sequence: u32,
    pub audio: Vec<u8>,
    pub is_final: bool,
    pub error: Option<String>,
}

// ── Wire encoding ────────────────────────────────────────────────

impl Message {
    /// Encode as length-prefixed JSON.
    pub fn encode(&self) -> Vec<u8> {
        let data = serde_json::to_vec(self).expect("Message serialize");
        let len = (data.len() as u32).to_le_bytes();
        [len.as_slice(), &data].concat()
    }

    /// Decode from length-prefixed buffer. Returns (message, bytes_consumed).
    pub fn decode(bytes: &[u8]) -> Result<(Self, usize), &'static str> {
        if bytes.len() < 4 {
            return Err("not enough bytes for length");
        }
        let len = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
        if bytes.len() < 4 + len {
            return Err("not enough bytes for message");
        }
        let msg: Self = serde_json::from_slice(&bytes[4..4 + len])
            .map_err(|_| "decode failed")?;
        Ok((msg, 4 + len))
    }
}
