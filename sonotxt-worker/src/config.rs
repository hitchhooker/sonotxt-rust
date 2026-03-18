use clap::Parser;

#[derive(Parser, Debug, Clone)]
#[command(name = "sonotxt-worker")]
#[command(about = "TTS worker (GPU-side, QUIC-only)", long_about = None)]
pub struct WorkerConfig {
    #[arg(long, env = "RUST_LOG", default_value = "info")]
    pub log_level: String,

    /// QUIC server port (Noise_NK encrypted transport for API connections)
    #[arg(long, env = "QUIC_PORT", default_value = "4433")]
    pub quic_port: u16,

    /// HTTP health server port (for legacy monitoring)
    #[arg(long, env = "HEALTH_PORT", default_value = "9090")]
    pub health_port: u16,

    // Local python service URLs
    #[arg(long, env = "SPEECH_URL", default_value = "http://127.0.0.1:8080")]
    pub speech_url: String,

    #[arg(long, env = "LLM_URL", default_value = "http://127.0.0.1:8090")]
    pub llm_url: String,

    /// API key for speech service (Authorization: Bearer <key>)
    #[arg(long, env = "SPEECH_API_KEY")]
    pub speech_api_key: Option<String>,
}

impl WorkerConfig {
    pub fn from_env() -> Self {
        dotenvy::dotenv().ok();
        Self::parse()
    }
}
