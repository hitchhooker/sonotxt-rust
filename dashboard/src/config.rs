use clap::Parser;

#[derive(Parser, Debug, Clone)]
#[command(name = "sonotxt")]
#[command(about = "TTS API server", long_about = None)]
pub struct Config {
    #[arg(long, env = "REDIS_URL", default_value = "redis://127.0.0.1:6379")]
    pub redis_url: String,

    #[arg(long, env = "SERVER_HOST", default_value = "0.0.0.0")]
    pub host: String,

    #[arg(long, env = "SERVER_PORT", default_value = "8080")]
    pub port: u16,

    #[arg(long, env = "RUST_LOG", default_value = "info")]
    pub log_level: String,

    #[arg(long, env = "COST_PER_CHAR", default_value = "0.00001")]
    pub cost_per_char: f64,

    #[arg(long, env = "MAX_CONTENT_SIZE", default_value = "50000")]
    pub max_content_size: usize,

    #[arg(long, env = "REQUEST_TIMEOUT_SECS", default_value = "30")]
    pub request_timeout: u64,

    #[arg(long, env = "S3_BUCKET", default_value = "sonotxt-audio")]
    pub s3_bucket: String,

    #[arg(long, env = "ADMIN_TOKEN")]
    pub admin_token: Option<String>,

    #[arg(long, env = "DATABASE_URL", default_value = "postgres://localhost/sonotxt")]
    pub database_url: String,
}

impl Config {
    pub fn from_env() -> Self {
        dotenvy::dotenv().ok();
        Self::parse()
    }
}
