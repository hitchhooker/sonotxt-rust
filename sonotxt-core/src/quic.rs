//! QUIC transport utilities shared by API (client) and worker (server).

use quinn::crypto::rustls::QuicClientConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use std::net::SocketAddr;
use std::sync::Arc;

use crate::protocol::Message;

pub const ALPN: &[u8] = b"sonotxt-1";

/// Read one length-prefixed Message from a QUIC recv stream.
pub async fn read_message(recv: &mut quinn::RecvStream) -> Result<Message, Box<dyn std::error::Error + Send + Sync>> {
    let mut len_buf = [0u8; 4];
    recv.read_exact(&mut len_buf).await?;
    let msg_len = u32::from_le_bytes(len_buf) as usize;

    let mut body = vec![0u8; msg_len];
    recv.read_exact(&mut body).await?;

    // Reconstruct length-prefixed buffer for Message::decode
    let mut buf = len_buf.to_vec();
    buf.extend(body);

    let (msg, _) = Message::decode(&buf).map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;
    Ok(msg)
}

/// Write one Message to a QUIC send stream.
pub async fn write_message(send: &mut quinn::SendStream, msg: &Message) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    send.write_all(&msg.encode()).await?;
    Ok(())
}

// ── Server-side QUIC config ───────────────────────────────────

/// Generate self-signed cert and build quinn ServerConfig.
/// In production, attestation binding replaces cert trust.
pub fn server_config() -> Result<quinn::ServerConfig, Box<dyn std::error::Error + Send + Sync>> {
    let (cert, key) = generate_self_signed_cert()?;

    let mut server_crypto = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)?;

    server_crypto.alpn_protocols = vec![ALPN.to_vec()];

    let quic_config = quinn::crypto::rustls::QuicServerConfig::try_from(server_crypto)?;
    Ok(quinn::ServerConfig::with_crypto(Arc::new(quic_config)))
}

/// Create QUIC server endpoint.
pub fn server_endpoint(addr: SocketAddr) -> Result<quinn::Endpoint, Box<dyn std::error::Error + Send + Sync>> {
    let config = server_config()?;
    let endpoint = quinn::Endpoint::server(config, addr)?;
    Ok(endpoint)
}

// ── Client-side QUIC config ──────────────────────────────────

/// Create QUIC client endpoint.
/// Skips TLS cert verification — trust comes from Noise attestation binding, not TLS PKI.
pub fn client_endpoint() -> Result<quinn::Endpoint, Box<dyn std::error::Error + Send + Sync>> {
    let mut crypto = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(SkipServerVerification))
        .with_no_client_auth();

    crypto.alpn_protocols = vec![ALPN.to_vec()];

    let client_config = quinn::ClientConfig::new(Arc::new(
        QuicClientConfig::try_from(crypto)?
    ));

    let mut endpoint = quinn::Endpoint::client("0.0.0.0:0".parse()?)?;
    endpoint.set_default_client_config(client_config);
    Ok(endpoint)
}

// ── Certificate generation ───────────────────────────────────

fn generate_self_signed_cert() -> Result<(CertificateDer<'static>, PrivateKeyDer<'static>), Box<dyn std::error::Error + Send + Sync>> {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])?;
    let key = PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der()).into();
    let cert = CertificateDer::from(cert.cert);
    Ok((cert, key))
}

/// Skip TLS cert verification. Trust is established through Noise_NK
/// attestation binding, not through the TLS certificate chain.
#[derive(Debug)]
struct SkipServerVerification;

impl rustls::client::danger::ServerCertVerifier for SkipServerVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        vec![
            rustls::SignatureScheme::RSA_PKCS1_SHA256,
            rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
            rustls::SignatureScheme::RSA_PSS_SHA256,
            rustls::SignatureScheme::ED25519,
        ]
    }
}
