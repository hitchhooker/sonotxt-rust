//! Noise_NK_25519_ChaChaPoly_SHA256 session management.
//!
//! NK pattern: the client knows the server's static key (from attestation).
//! Server authenticates to client; client is anonymous (API identity comes
//! from the job queue, not from the transport).
//!
//! Large messages are chunked to fit Noise's 65535-byte frame limit.

use snow::{Builder, HandshakeState, Keypair, TransportState};
use std::collections::HashMap;

const NOISE_PATTERN: &str = "Noise_NK_25519_ChaChaPoly_SHA256";
/// Leave room for auth tag (16 bytes) + overhead
const MAX_CHUNK_SIZE: usize = 65000;

// ── Client session ─────────────────────────────────────────────

/// Noise client (API side). One per QUIC connection.
pub struct NoiseClient {
    handshake: Option<HandshakeState>,
    transport: Option<TransportState>,
    session_id: Option<[u8; 16]>,
}

impl NoiseClient {
    pub fn new() -> Self {
        Self {
            handshake: None,
            transport: None,
            session_id: None,
        }
    }

    /// Start handshake with server's static public key (from attestation).
    /// Returns the handshake message to send.
    pub fn initiate_handshake(
        &mut self,
        server_static_key: &[u8],
    ) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
        let builder = Builder::new(NOISE_PATTERN.parse()?);
        let mut initiator = builder
            .remote_public_key(server_static_key)
            .build_initiator()?;

        let mut message = vec![0u8; 65535];
        let len = initiator.write_message(&[], &mut message)?;
        message.truncate(len);

        self.handshake = Some(initiator);
        Ok(message)
    }

    /// Complete handshake with server's response.
    pub fn complete_handshake(
        &mut self,
        server_response: &[u8],
        session_id: [u8; 16],
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut initiator = self
            .handshake
            .take()
            .ok_or("no handshake in progress")?;

        let mut payload = vec![0u8; 65535];
        let _len = initiator.read_message(server_response, &mut payload)?;

        self.transport = Some(initiator.into_transport_mode()?);
        self.session_id = Some(session_id);
        Ok(())
    }

    /// Encrypt plaintext → chunked ciphertext.
    pub fn encrypt(
        &mut self,
        plaintext: &[u8],
    ) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
        let transport = self.transport.as_mut().ok_or("session not established")?;
        encrypt_chunked(transport, plaintext)
    }

    /// Decrypt chunked ciphertext → plaintext.
    pub fn decrypt(
        &mut self,
        data: &[u8],
    ) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
        let transport = self.transport.as_mut().ok_or("session not established")?;
        decrypt_chunked(transport, data)
    }

    pub fn session_id(&self) -> Option<[u8; 16]> {
        self.session_id
    }
}

// ── Server session manager ─────────────────────────────────────

/// Noise server (worker side). Manages multiple client sessions.
pub struct NoiseServer {
    static_keypair: Keypair,
    sessions: HashMap<[u8; 16], TransportState>,
}

impl NoiseServer {
    pub fn new() -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let builder = Builder::new(NOISE_PATTERN.parse()?);
        let static_keypair = builder.generate_keypair()?;

        Ok(Self {
            static_keypair,
            sessions: HashMap::new(),
        })
    }

    /// Static public key for attestation binding.
    pub fn static_public_key(&self) -> &[u8] {
        &self.static_keypair.public
    }

    /// Process client handshake. Returns (session_id, response_message).
    pub fn process_handshake(
        &mut self,
        client_msg: &[u8],
    ) -> Result<([u8; 16], Vec<u8>), Box<dyn std::error::Error + Send + Sync>> {
        let builder = Builder::new(NOISE_PATTERN.parse()?);
        let mut responder = builder
            .local_private_key(&self.static_keypair.private)
            .build_responder()?;

        let mut payload = vec![0u8; 65535];
        let _len = responder.read_message(client_msg, &mut payload)?;

        let mut response = vec![0u8; 65535];
        let len = responder.write_message(&[], &mut response)?;
        response.truncate(len);

        let transport = responder.into_transport_mode()?;

        let mut session_id = [0u8; 16];
        rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut session_id);

        self.sessions.insert(session_id, transport);
        Ok((session_id, response))
    }

    pub fn decrypt(
        &mut self,
        session_id: &[u8; 16],
        data: &[u8],
    ) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
        let transport = self.sessions.get_mut(session_id).ok_or("session not found")?;
        decrypt_chunked(transport, data)
    }

    pub fn encrypt(
        &mut self,
        session_id: &[u8; 16],
        plaintext: &[u8],
    ) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
        let transport = self.sessions.get_mut(session_id).ok_or("session not found")?;
        encrypt_chunked(transport, plaintext)
    }

    pub fn remove_session(&mut self, session_id: &[u8; 16]) {
        self.sessions.remove(session_id);
    }
}

// ── Chunked encryption/decryption ──────────────────────────────

fn encrypt_chunked(
    transport: &mut TransportState,
    plaintext: &[u8],
) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
    let mut result = Vec::new();
    let chunks: Vec<&[u8]> = plaintext.chunks(MAX_CHUNK_SIZE).collect();
    let num_chunks = chunks.len() as u32;

    result.extend_from_slice(&num_chunks.to_le_bytes());

    for chunk in chunks {
        let mut ciphertext = vec![0u8; chunk.len() + 16];
        let len = transport.write_message(chunk, &mut ciphertext)?;
        ciphertext.truncate(len);

        result.extend_from_slice(&(ciphertext.len() as u32).to_le_bytes());
        result.extend(ciphertext);
    }

    Ok(result)
}

fn decrypt_chunked(
    transport: &mut TransportState,
    data: &[u8],
) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
    if data.len() < 4 {
        return Err("invalid encrypted data".into());
    }

    let num_chunks = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
    let mut offset = 4;
    let mut result = Vec::new();

    for _ in 0..num_chunks {
        if offset + 4 > data.len() {
            return Err("truncated chunk header".into());
        }

        let chunk_len = u32::from_le_bytes([
            data[offset],
            data[offset + 1],
            data[offset + 2],
            data[offset + 3],
        ]) as usize;
        offset += 4;

        if offset + chunk_len > data.len() {
            return Err("truncated chunk data".into());
        }

        let chunk = &data[offset..offset + chunk_len];
        offset += chunk_len;

        let mut plaintext = vec![0u8; chunk.len()];
        let len = transport.read_message(chunk, &mut plaintext)?;
        plaintext.truncate(len);

        result.extend(plaintext);
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_noise_session_roundtrip() {
        let mut server = NoiseServer::new().unwrap();
        let mut client = NoiseClient::new();

        // Client initiates with server's public key
        let client_msg = client
            .initiate_handshake(server.static_public_key())
            .unwrap();

        // Server processes handshake
        let (session_id, server_response) = server.process_handshake(&client_msg).unwrap();

        // Client completes handshake
        client
            .complete_handshake(&server_response, session_id)
            .unwrap();

        // Client encrypts, server decrypts
        let plaintext = b"hello from API";
        let ciphertext = client.encrypt(plaintext).unwrap();
        let decrypted = server.decrypt(&session_id, &ciphertext).unwrap();
        assert_eq!(decrypted, plaintext);

        // Server encrypts, client decrypts
        let response = b"audio data here";
        let encrypted = server.encrypt(&session_id, response).unwrap();
        let decrypted = client.decrypt(&encrypted).unwrap();
        assert_eq!(decrypted, response);
    }

    #[test]
    fn test_large_message_chunking() {
        let mut server = NoiseServer::new().unwrap();
        let mut client = NoiseClient::new();

        let client_msg = client
            .initiate_handshake(server.static_public_key())
            .unwrap();
        let (session_id, server_response) = server.process_handshake(&client_msg).unwrap();
        client
            .complete_handshake(&server_response, session_id)
            .unwrap();

        // 200KB message (forces multiple chunks)
        let big_data = vec![0xAB; 200_000];
        let encrypted = client.encrypt(&big_data).unwrap();
        let decrypted = server.decrypt(&session_id, &encrypted).unwrap();
        assert_eq!(decrypted, big_data);
    }
}
