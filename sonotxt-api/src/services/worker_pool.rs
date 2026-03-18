//! Your Server as a Function — Eriksen (Twitter, 2013)
//!
//! Service[Req, Rep] = async fn(Req) -> Result<Rep>
//! Filter[Req, Rep]  = fn(Req, Service) -> Future<Rep>
//!
//! Filters compose with `and_then` to build services from independent modules:
//!
//!   let tts = load_balance(workers)
//!       .and_then(inflight_tracker)
//!       .and_then(timeout(180s))
//!       .and_then(retry(2))
//!       .and_then(metrics("tts"))
//!       .and_then(backup_request(p99))
//!
//! All GPU worker communication flows through this module.
//! Callers never touch HTTP directly — they call a Service.
//!
//! Transport: QUIC + Noise_NK (primary), HTTP (fallback).
//! QUIC connections are established on init and maintained with health checks.

use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::{error, info, warn};

use crate::services::quic_pool::QuicWorkerConn;

// Re-export wire types from core so existing imports still resolve
pub use sonotxt_core::{
    ServiceError, TtsRequest, TtsResponse, AsrRequest, AsrResponse,
    LlmRequest, LlmResponse, LlmMessage,
};

// ── Service trait ──────────────────────────────────────────────────

/// The core abstraction: an async function from Req to Rep.
/// `type Service[Req, Rep] = Req => Future[Rep]`
pub trait Service<Req: Send + 'static, Rep: Send + 'static>: Send + Sync + 'static {
    fn call(&self, req: Req) -> Pin<Box<dyn Future<Output = Result<Rep, ServiceError>> + Send>>;
}

/// Wrap any async fn as a Service.
impl<F, Req, Rep, Fut> Service<Req, Rep> for F
where
    F: Fn(Req) -> Fut + Send + Sync + 'static,
    Req: Send + 'static,
    Rep: Send + 'static,
    Fut: Future<Output = Result<Rep, ServiceError>> + Send + 'static,
{
    fn call(&self, req: Req) -> Pin<Box<dyn Future<Output = Result<Rep, ServiceError>> + Send>> {
        Box::pin((self)(req))
    }
}

// ── Worker ─────────────────────────────────────────────────────────

pub struct Worker {
    pub speech_url: String,
    pub llm_url: String,
    pub healthy: AtomicBool,
    pub inflight: AtomicU64,
    pub total_requests: AtomicU64,
    pub total_failures: AtomicU64,
    pub last_latency_ms: AtomicU64,
    /// QUIC+Noise connection (primary transport). None if connection failed.
    pub quic: RwLock<Option<QuicWorkerConn>>,
}

impl std::fmt::Debug for Worker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Worker")
            .field("speech_url", &self.speech_url)
            .field("llm_url", &self.llm_url)
            .field("healthy", &self.healthy.load(Ordering::Relaxed))
            .finish()
    }
}

// ── Concrete Services (the leaf nodes) ─────────────────────────────

/// TTS Service: sends text to a worker, gets back audio.
pub struct TtsService {
    http: Client,
    worker: Arc<Worker>,
}

impl Service<TtsRequest, TtsResponse> for TtsService {
    fn call(&self, req: TtsRequest) -> Pin<Box<dyn Future<Output = Result<TtsResponse, ServiceError>> + Send>> {
        let http = self.http.clone();
        let url = format!("{}/synthesize", self.worker.speech_url);
        let worker = self.worker.clone();

        Box::pin(async move {
            worker.inflight.fetch_add(1, Ordering::Relaxed);
            worker.total_requests.fetch_add(1, Ordering::Relaxed);
            let start = Instant::now();

            #[derive(Serialize)]
            struct Body { text: String, speaker: String, language: String }

            let mut builder = http.post(&url)
                .header("Content-Type", "application/json");
            if let Some(ref key) = req.api_key {
                builder = builder.header("Authorization", format!("Bearer {}", key));
            }

            let result = builder
                .json(&Body { text: req.text, speaker: req.speaker, language: req.language })
                .send()
                .await;

            worker.inflight.fetch_sub(1, Ordering::Relaxed);
            let runtime_ms = start.elapsed().as_millis() as u64;

            let response = result.map_err(|e| {
                worker.total_failures.fetch_add(1, Ordering::Relaxed);
                ServiceError::Failed(format!("http: {}", e))
            })?;

            if !response.status().is_success() {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                worker.total_failures.fetch_add(1, Ordering::Relaxed);
                return Err(ServiceError::Failed(format!("tts {}: {}", status, body)));
            }

            let wav_data = response.bytes().await.map_err(|e| {
                ServiceError::Failed(format!("read body: {}", e))
            })?;

            // Parse WAV header for duration
            let duration_seconds = if wav_data.len() > 44 {
                let sr = u32::from_le_bytes([wav_data[24], wav_data[25], wav_data[26], wav_data[27]]);
                let ds = u32::from_le_bytes([wav_data[40], wav_data[41], wav_data[42], wav_data[43]]);
                ds as f64 / (sr as f64 * 2.0)
            } else {
                0.0
            };

            Ok(TtsResponse {
                audio_data: wav_data.to_vec(),
                format: "wav".to_string(),
                duration_seconds,
                runtime_ms,
            })
        })
    }
}

/// ASR Service: sends audio to a worker, gets back text.
pub struct AsrService {
    http: Client,
    worker: Arc<Worker>,
}

impl Service<AsrRequest, AsrResponse> for AsrService {
    fn call(&self, req: AsrRequest) -> Pin<Box<dyn Future<Output = Result<AsrResponse, ServiceError>> + Send>> {
        let http = self.http.clone();
        let url = format!("{}/transcribe_base64", self.worker.speech_url);
        let worker = self.worker.clone();

        Box::pin(async move {
            worker.inflight.fetch_add(1, Ordering::Relaxed);

            #[derive(Serialize)]
            struct Body { audio_base64: String }
            #[derive(Deserialize)]
            struct Resp { text: String }

            let result = http.post(&url)
                .json(&Body { audio_base64: req.audio_base64 })
                .send()
                .await;

            worker.inflight.fetch_sub(1, Ordering::Relaxed);

            let response = result.map_err(|e| ServiceError::Failed(format!("http: {}", e)))?;
            if !response.status().is_success() {
                return Err(ServiceError::Failed(format!("asr: {}", response.status())));
            }

            let resp: Resp = response.json().await.map_err(|e| ServiceError::Failed(format!("json: {}", e)))?;
            Ok(AsrResponse { text: resp.text })
        })
    }
}

/// LLM Service: sends messages to a worker, gets back sentences.
pub struct LlmService {
    http: Client,
    worker: Arc<Worker>,
}

impl Service<LlmRequest, LlmResponse> for LlmService {
    fn call(&self, req: LlmRequest) -> Pin<Box<dyn Future<Output = Result<LlmResponse, ServiceError>> + Send>> {
        let http = self.http.clone();
        let url = format!("{}/chat_sentences", self.worker.llm_url);
        let worker = self.worker.clone();

        Box::pin(async move {
            worker.inflight.fetch_add(1, Ordering::Relaxed);
            let _start = Instant::now();

            #[derive(Serialize)]
            struct Body { messages: Vec<LlmMessage>, max_tokens: u32, temperature: f64 }
            #[derive(Deserialize)]
            struct Resp { sentences: Vec<String>, full_response: String, tokens: Option<u32> }

            let result = http.post(&url)
                .json(&Body { messages: req.messages, max_tokens: req.max_tokens, temperature: req.temperature })
                .send()
                .await;

            worker.inflight.fetch_sub(1, Ordering::Relaxed);
            let runtime_ms = _start.elapsed().as_millis() as u64;

            let response = result.map_err(|e| ServiceError::Failed(format!("http: {}", e)))?;
            if !response.status().is_success() {
                return Err(ServiceError::Failed(format!("llm: {}", response.status())));
            }

            let resp: Resp = response.json().await.map_err(|e| ServiceError::Failed(format!("json: {}", e)))?;
            Ok(LlmResponse {
                sentences: resp.sentences,
                full_response: resp.full_response,
                tokens: resp.tokens.unwrap_or(0),
                runtime_ms,
            })
        })
    }
}

// ── WorkerPool: the composed service ──────────────────────────────

/// The pool IS the service. Callers don't pick workers manually —
/// they call pool.tts(), pool.asr(), pool.llm() and get back results.
/// Routing, health, timeouts, retries are all internal.
///
/// Transport priority: QUIC+Noise (encrypted, fast) → HTTP (fallback).
pub struct WorkerPool {
    workers: Vec<Arc<Worker>>,
    http: Client,
    rr_counter: AtomicU64,
    tts_timeout: Duration,
    asr_timeout: Duration,
    llm_timeout: Duration,
    max_retries: u32,
}

/// Health response from HTTP /health (fallback)
#[derive(Deserialize, Debug)]
struct HealthResponse {
    tts_loaded: Option<bool>,
    model_loaded: Option<bool>,
}

impl WorkerPool {
    /// Create pool and connect QUIC to each worker.
    /// QUIC connections are best-effort — workers that don't respond
    /// fall back to HTTP.
    pub async fn new(urls: &str, http: Client) -> Self {
        let workers: Vec<Arc<Worker>> = urls
            .split(',')
            .map(|u| u.trim())
            .filter(|u| !u.is_empty())
            .map(|speech_url| {
                // Derive LLM URL: same host, port + 10
                // http://1.2.3.4:8080 → http://1.2.3.4:8090
                // http://127.0.0.1:28080 → http://127.0.0.1:28090
                let llm_url = if let Some(colon_pos) = speech_url.rfind(':') {
                    if let Ok(port) = speech_url[colon_pos + 1..].parse::<u16>() {
                        format!("{}:{}", &speech_url[..colon_pos], port + 10)
                    } else {
                        speech_url.replace(":8080", ":8090")
                    }
                } else {
                    speech_url.replace(":8080", ":8090")
                };
                Arc::new(Worker {
                    speech_url: speech_url.to_string(),
                    llm_url,
                    healthy: AtomicBool::new(true),
                    inflight: AtomicU64::new(0),
                    total_requests: AtomicU64::new(0),
                    total_failures: AtomicU64::new(0),
                    last_latency_ms: AtomicU64::new(0),
                    quic: RwLock::new(None),
                })
            })
            .collect();

        info!("worker pool: {} workers", workers.len());

        // QUIC connections are established when workers have direct UDP access.
        // For SSH-tunneled workers (localhost), skip QUIC and use HTTP only.
        for worker in &workers {
            if !worker.speech_url.contains("127.0.0.1") && !worker.speech_url.contains("localhost") {
                let w = worker.clone();
                tokio::spawn(async move {
                    let quic_addr = derive_quic_addr(&w.speech_url);
                    match QuicWorkerConn::connect(quic_addr).await {
                        Ok(conn) => {
                            info!("QUIC connected: {} → {}", w.speech_url, quic_addr);
                            *w.quic.write().await = Some(conn);
                        }
                        Err(e) => {
                            warn!("QUIC connect failed for {} (HTTP fallback): {}", w.speech_url, e);
                        }
                    }
                });
            }
        }

        for w in &workers {
            info!("  speech={} llm={}", w.speech_url, w.llm_url);
        }

        Self {
            workers,
            http,
            rr_counter: AtomicU64::new(0),
            tts_timeout: Duration::from_secs(180),
            asr_timeout: Duration::from_secs(30),
            llm_timeout: Duration::from_secs(60),
            max_retries: 1,
        }
    }

    // ── Composed service calls ──────────────────────────────────

    /// TTS: text → audio. Load balanced, with timeout and retry.
    /// `loadBalance andThen timeout(180s) andThen retry(1)`
    pub async fn tts(&self, req: TtsRequest) -> Result<TtsResponse, ServiceError> {
        self.with_retry(|worker| {
            let svc = TtsService { http: self.http.clone(), worker: worker.clone() };
            let req = req.clone();
            async move {
                tokio::time::timeout(self.tts_timeout, svc.call(req))
                    .await
                    .map_err(|_| ServiceError::Timeout)?
            }
        }).await
    }

    /// Encrypted TTS: text encrypted end-to-end via Noise channel.
    /// Text never hits disk on the worker. For private inference.
    pub async fn encrypted_tts(
        &self,
        text: &str,
        voice: &str,
        language: &str,
    ) -> Result<TtsResponse, ServiceError> {
        let worker = self.pick().ok_or(ServiceError::Unavailable)?;
        let quic_guard = worker.quic.read().await;
        let quic = quic_guard.as_ref().ok_or(ServiceError::Unavailable)?;

        let mut request_id = [0u8; 16];
        rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut request_id);

        let request = sonotxt_core::EncryptedTtsRequest {
            request_id,
            text: text.to_string(),
            voice: voice.to_string(),
            speed: 1.0,
            language: language.to_string(),
        };

        worker.inflight.fetch_add(1, Ordering::Relaxed);
        worker.total_requests.fetch_add(1, Ordering::Relaxed);
        let start = Instant::now();

        let result = tokio::time::timeout(
            self.tts_timeout,
            quic.encrypted_tts(&request),
        ).await;

        worker.inflight.fetch_sub(1, Ordering::Relaxed);

        let response = result
            .map_err(|_| ServiceError::Timeout)?
            .map_err(|e| {
                worker.total_failures.fetch_add(1, Ordering::Relaxed);
                ServiceError::Failed(format!("quic tts: {}", e))
            })?;

        if let Some(err) = response.error {
            worker.total_failures.fetch_add(1, Ordering::Relaxed);
            return Err(ServiceError::Failed(err));
        }

        let runtime_ms = start.elapsed().as_millis() as u64;
        worker.last_latency_ms.store(runtime_ms, Ordering::Relaxed);

        Ok(TtsResponse {
            audio_data: response.audio,
            format: response.format,
            duration_seconds: response.duration_seconds,
            runtime_ms,
        })
    }

    /// Push job notification to a worker over QUIC.
    /// Falls back silently if no QUIC connection (redis/poll will catch it).
    pub async fn notify_job(&self, job_id: &str) {
        // Notify all connected workers (they compete via SELECT FOR UPDATE)
        for worker in &self.workers {
            let quic_guard = worker.quic.read().await;
            if let Some(ref quic) = *quic_guard {
                if let Err(e) = quic.notify_job(job_id).await {
                    warn!("QUIC job notify failed for {}: {}", worker.speech_url, e);
                }
            }
        }
    }

    /// ASR: audio → text. Tries QUIC (encrypted), falls back to HTTP.
    pub async fn asr(&self, req: AsrRequest) -> Result<AsrResponse, ServiceError> {
        let worker = self.pick().ok_or(ServiceError::Unavailable)?;

        // Try QUIC first
        {
            let quic_guard = worker.quic.read().await;
            if let Some(ref quic) = *quic_guard {
                match tokio::time::timeout(
                    self.asr_timeout,
                    quic.encrypted_asr(&req.audio_base64),
                ).await {
                    Ok(Ok(resp)) => {
                        if let Some(err) = resp.error {
                            return Err(ServiceError::Failed(err));
                        }
                        return Ok(AsrResponse { text: resp.text });
                    }
                    Ok(Err(e)) => warn!("QUIC ASR failed, HTTP fallback: {}", e),
                    Err(_) => warn!("QUIC ASR timeout, HTTP fallback"),
                }
            }
        }

        // HTTP fallback
        let svc = AsrService { http: self.http.clone(), worker };
        tokio::time::timeout(self.asr_timeout, svc.call(req))
            .await
            .map_err(|_| ServiceError::Timeout)?
    }

    /// LLM: messages → sentences. Load balanced, with timeout.
    pub async fn llm(&self, req: LlmRequest) -> Result<LlmResponse, ServiceError> {
        let worker = self.pick().ok_or(ServiceError::Unavailable)?;
        let svc = LlmService { http: self.http.clone(), worker };
        tokio::time::timeout(self.llm_timeout, svc.call(req))
            .await
            .map_err(|_| ServiceError::Timeout)?
    }

    /// LLM streaming: returns a byte stream from /chat_stream (SSE).
    pub async fn llm_stream(&self, req: LlmRequest) -> Result<reqwest::Response, ServiceError> {
        let worker = self.pick().ok_or(ServiceError::Unavailable)?;
        worker.inflight.fetch_add(1, Ordering::Relaxed);

        #[derive(Serialize)]
        struct Body { messages: Vec<LlmMessage>, max_tokens: u32, temperature: f64 }

        let result = self.http
            .post(format!("{}/chat_stream", worker.llm_url))
            .json(&Body { messages: req.messages, max_tokens: req.max_tokens, temperature: req.temperature })
            .send()
            .await;

        worker.inflight.fetch_sub(1, Ordering::Relaxed);

        let resp = result.map_err(|e| ServiceError::Failed(format!("http: {}", e)))?;
        if !resp.status().is_success() {
            return Err(ServiceError::Failed(format!("llm_stream: {}", resp.status())));
        }
        Ok(resp)
    }

    /// Backup request pattern (Eriksen Appendix A).
    pub async fn tts_with_backup(&self, req: TtsRequest) -> Result<TtsResponse, ServiceError> {
        let primary = self.pick().ok_or(ServiceError::Unavailable)?;
        let backup_worker = self.pick_different(&primary);

        let primary_svc = TtsService { http: self.http.clone(), worker: primary.clone() };
        let primary_req = req.clone();

        if let Some(backup) = backup_worker {
            let cutoff = Duration::from_millis(primary.last_latency_ms.load(Ordering::Relaxed).max(500));
            let backup_svc = TtsService { http: self.http.clone(), worker: backup };
            let backup_req = req;

            let primary_fut = primary_svc.call(primary_req);

            tokio::select! {
                result = primary_fut => result,
                _ = tokio::time::sleep(cutoff) => {
                    info!("backup request fired after {}ms cutoff", cutoff.as_millis());
                    backup_svc.call(backup_req).await
                }
            }
        } else {
            tokio::time::timeout(self.tts_timeout, primary_svc.call(primary_req))
                .await
                .map_err(|_| ServiceError::Timeout)?
        }
    }

    // ── Retry logic ────────────────────────────────────────────

    async fn with_retry<F, Fut>(&self, f: F) -> Result<TtsResponse, ServiceError>
    where
        F: Fn(Arc<Worker>) -> Fut,
        Fut: Future<Output = Result<TtsResponse, ServiceError>>,
    {
        let mut last_err = ServiceError::Unavailable;

        for attempt in 0..=self.max_retries {
            let worker = self.pick().ok_or(ServiceError::Unavailable)?;

            match f(worker.clone()).await {
                Ok(resp) => return Ok(resp),
                Err(e) => {
                    worker.total_failures.fetch_add(1, Ordering::Relaxed);
                    warn!("attempt {}: {} failed: {}", attempt, worker.speech_url, e);
                    last_err = e;
                    if attempt < self.max_retries {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                }
            }
        }

        Err(last_err)
    }

    // ── Load balancing ─────────────────────────────────────────

    /// Least-loaded among healthy workers, round-robin tiebreak.
    pub fn pick(&self) -> Option<Arc<Worker>> {
        let healthy: Vec<_> = self.workers.iter()
            .filter(|w| w.healthy.load(Ordering::Relaxed))
            .collect();

        let pool = if healthy.is_empty() {
            warn!("no healthy workers, trying all");
            &self.workers
        } else {
            return self.pick_from(&healthy);
        };

        if pool.is_empty() { return None; }
        let idx = self.rr_counter.fetch_add(1, Ordering::Relaxed) as usize % pool.len();
        Some(pool[idx].clone())
    }

    fn pick_from(&self, workers: &[&Arc<Worker>]) -> Option<Arc<Worker>> {
        if workers.is_empty() { return None; }
        let min_load = workers.iter().map(|w| w.inflight.load(Ordering::Relaxed)).min().unwrap_or(0);
        let least: Vec<_> = workers.iter()
            .filter(|w| w.inflight.load(Ordering::Relaxed) == min_load)
            .collect();
        let idx = self.rr_counter.fetch_add(1, Ordering::Relaxed) as usize % least.len();
        Some((*least[idx]).clone())
    }

    fn pick_different(&self, exclude: &Arc<Worker>) -> Option<Arc<Worker>> {
        let others: Vec<_> = self.workers.iter()
            .filter(|w| w.healthy.load(Ordering::Relaxed) && !Arc::ptr_eq(w, exclude))
            .collect();
        if others.is_empty() { return None; }
        let idx = self.rr_counter.fetch_add(1, Ordering::Relaxed) as usize % others.len();
        Some(others[idx].clone())
    }

    // ── Health checking ────────────────────────────────────────

    /// Health check all workers. Prefers QUIC (faster), falls back to HTTP.
    pub async fn health_check(&self) {
        for worker in &self.workers {
            let start = Instant::now();

            // Try QUIC health first
            let quic_health = {
                let quic_guard = worker.quic.read().await;
                if let Some(ref quic) = *quic_guard {
                    quic.health().await.ok()
                } else {
                    None
                }
            };

            let (speech_ok, llm_ok) = if let Some(health) = quic_health {
                (health.speech_ok, health.llm_ok)
            } else {
                // HTTP fallback
                let speech_ok = self.check_health_http(&worker.speech_url, "speech").await;
                let llm_ok = self.check_health_http(&worker.llm_url, "llm").await;
                (speech_ok, llm_ok)
            };

            worker.last_latency_ms.store(start.elapsed().as_millis() as u64, Ordering::Relaxed);

            let was = worker.healthy.load(Ordering::Relaxed);
            let now = speech_ok && llm_ok;
            worker.healthy.store(now, Ordering::Relaxed);

            if was && !now { warn!("worker {} DOWN", worker.speech_url); }
            if !was && now { info!("worker {} recovered", worker.speech_url); }

            // QUIC reconnect is disabled for SSH-tunneled workers (no UDP path).
            // When workers have direct UDP access, re-enable this.
        }
    }

    async fn check_health_http(&self, base_url: &str, label: &str) -> bool {
        let resp = self.http.get(format!("{}/health", base_url))
            .timeout(Duration::from_secs(5))
            .send()
            .await;
        match resp {
            Ok(r) if r.status().is_success() => {
                if let Ok(h) = r.json::<HealthResponse>().await {
                    match label {
                        "speech" => h.tts_loaded.unwrap_or(false),
                        "llm" => h.model_loaded.unwrap_or(false),
                        _ => true,
                    }
                } else { false }
            }
            _ => false,
        }
    }

    // ── Monitoring ─────────────────────────────────────────────

    pub fn status(&self) -> Vec<WorkerStatus> {
        self.workers.iter().map(|w| WorkerStatus {
            speech_url: w.speech_url.clone(),
            llm_url: w.llm_url.clone(),
            healthy: w.healthy.load(Ordering::Relaxed),
            inflight: w.inflight.load(Ordering::Relaxed),
            total_requests: w.total_requests.load(Ordering::Relaxed),
            total_failures: w.total_failures.load(Ordering::Relaxed),
            latency_ms: w.last_latency_ms.load(Ordering::Relaxed),
        }).collect()
    }

    pub fn healthy_count(&self) -> usize {
        self.workers.iter().filter(|w| w.healthy.load(Ordering::Relaxed)).count()
    }

    pub fn len(&self) -> usize {
        self.workers.len()
    }
}

/// Derive QUIC address from speech HTTP URL.
/// `http://1.2.3.4:8080` → `1.2.3.4:4433`
fn derive_quic_addr(speech_url: &str) -> std::net::SocketAddr {
    let stripped = speech_url
        .trim_start_matches("http://")
        .trim_start_matches("https://");
    let host = stripped.split(':').next().unwrap_or("127.0.0.1");
    format!("{}:4433", host).parse().unwrap_or_else(|_| {
        warn!("failed to parse QUIC addr from {}, using localhost", speech_url);
        "127.0.0.1:4433".parse().unwrap()
    })
}

#[derive(Serialize, Debug)]
pub struct WorkerStatus {
    pub speech_url: String,
    pub llm_url: String,
    pub healthy: bool,
    pub inflight: u64,
    pub total_requests: u64,
    pub total_failures: u64,
    pub latency_ms: u64,
}
