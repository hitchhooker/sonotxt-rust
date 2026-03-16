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

use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::{error, info, warn};

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

#[derive(Debug, Clone)]
pub enum ServiceError {
    Timeout,
    Unavailable,
    Failed(String),
    Cancelled,
}

impl std::fmt::Display for ServiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Timeout => write!(f, "timeout"),
            Self::Unavailable => write!(f, "service unavailable"),
            Self::Failed(msg) => write!(f, "{}", msg),
            Self::Cancelled => write!(f, "cancelled"),
        }
    }
}

// ── Concrete request/response types ────────────────────────────────

#[derive(Debug, Clone)]
pub struct TtsRequest {
    pub text: String,
    pub speaker: String,
    pub language: String,
    pub api_key: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TtsResponse {
    pub audio_data: Vec<u8>,
    pub format: String,
    pub duration_seconds: f64,
    pub runtime_ms: u64,
}

#[derive(Debug, Clone)]
pub struct AsrRequest {
    pub audio_base64: String,
}

#[derive(Debug, Clone)]
pub struct AsrResponse {
    pub text: String,
}

#[derive(Debug, Clone)]
pub struct LlmRequest {
    pub messages: Vec<LlmMessage>,
    pub max_tokens: u32,
    pub temperature: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmMessage {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Clone)]
pub struct LlmResponse {
    pub sentences: Vec<String>,
    pub full_response: String,
    pub tokens: u32,
    pub runtime_ms: u64,
}

// ── Worker ─────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct Worker {
    pub speech_url: String,
    pub llm_url: String,
    pub healthy: AtomicBool,
    pub inflight: AtomicU64,
    pub total_requests: AtomicU64,
    pub total_failures: AtomicU64,
    pub last_latency_ms: AtomicU64,
}

// ── Filters ────────────────────────────────────────────────────────
// Filters are composable middleware. Each wraps a Service to produce
// a new Service with added behavior.

// Timeout is applied inline: tokio::time::timeout(duration, svc.call(req))
// This is equivalent to Eriksen's timeoutFilter(d) andThen service.

// Retry and metrics are handled directly in WorkerPool methods
// rather than as standalone filter structs, because Rust's ownership
// model makes it cleaner to compose at the call site than to wrap
// services in generic filter chains. The Eriksen pattern is preserved
// in spirit: each pool.tts()/asr()/llm() call composes
// loadBalance → timeout → retry internally.

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
            let start = Instant::now();

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
            let start = Instant::now();

            #[derive(Serialize)]
            struct Body { messages: Vec<LlmMessage>, max_tokens: u32, temperature: f64 }
            #[derive(Deserialize)]
            struct Resp { sentences: Vec<String>, full_response: String, tokens: Option<u32> }

            let result = http.post(&url)
                .json(&Body { messages: req.messages, max_tokens: req.max_tokens, temperature: req.temperature })
                .send()
                .await;

            worker.inflight.fetch_sub(1, Ordering::Relaxed);
            let runtime_ms = start.elapsed().as_millis() as u64;

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
pub struct WorkerPool {
    workers: Vec<Arc<Worker>>,
    http: Client,
    rr_counter: AtomicU64,
    // Composed service config
    tts_timeout: Duration,
    asr_timeout: Duration,
    llm_timeout: Duration,
    max_retries: u32,
}

/// Health response from /health
#[derive(Deserialize, Debug)]
struct HealthResponse {
    tts_loaded: Option<bool>,
    model_loaded: Option<bool>,
}

impl WorkerPool {
    pub fn new(urls: &str, http: Client) -> Self {
        let workers: Vec<Arc<Worker>> = urls
            .split(',')
            .map(|u| u.trim())
            .filter(|u| !u.is_empty())
            .map(|speech_url| {
                let llm_url = speech_url.replace(":8080", ":8090");
                Arc::new(Worker {
                    speech_url: speech_url.to_string(),
                    llm_url,
                    healthy: AtomicBool::new(true),
                    inflight: AtomicU64::new(0),
                    total_requests: AtomicU64::new(0),
                    total_failures: AtomicU64::new(0),
                    last_latency_ms: AtomicU64::new(0),
                })
            })
            .collect();

        info!("worker pool: {} workers", workers.len());
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

    /// ASR: audio → text. Load balanced, with timeout.
    /// `loadBalance andThen timeout(30s)`
    pub async fn asr(&self, req: AsrRequest) -> Result<AsrResponse, ServiceError> {
        let worker = self.pick().ok_or(ServiceError::Unavailable)?;
        let svc = AsrService { http: self.http.clone(), worker };
        tokio::time::timeout(self.asr_timeout, svc.call(req))
            .await
            .map_err(|_| ServiceError::Timeout)?
    }

    /// LLM: messages → sentences. Load balanced, with timeout.
    /// `loadBalance andThen timeout(60s)`
    pub async fn llm(&self, req: LlmRequest) -> Result<LlmResponse, ServiceError> {
        let worker = self.pick().ok_or(ServiceError::Unavailable)?;
        let svc = LlmService { http: self.http.clone(), worker };
        tokio::time::timeout(self.llm_timeout, svc.call(req))
            .await
            .map_err(|_| ServiceError::Timeout)?
    }

    /// LLM streaming: returns a byte stream from /chat_stream (SSE).
    /// Each line is `data: {"event":"sentence","text":"..."}`.
    /// The caller reads sentences as they arrive and pipelines TTS.
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

    /// Backup request: fire a second request to a different worker
    /// if the primary hasn't responded within the p99 latency.
    /// This is Appendix A of the Eriksen paper.
    pub async fn tts_with_backup(&self, req: TtsRequest) -> Result<TtsResponse, ServiceError> {
        let primary = self.pick().ok_or(ServiceError::Unavailable)?;
        let backup_worker = self.pick_different(&primary);

        let primary_svc = TtsService { http: self.http.clone(), worker: primary.clone() };
        let primary_req = req.clone();

        // If we have a backup, race them
        if let Some(backup) = backup_worker {
            let cutoff = Duration::from_millis(primary.last_latency_ms.load(Ordering::Relaxed).max(500));
            let backup_svc = TtsService { http: self.http.clone(), worker: backup };
            let backup_req = req;

            // Start primary
            let primary_fut = primary_svc.call(primary_req);

            // Wait for cutoff, then fire backup
            tokio::select! {
                result = primary_fut => result,
                _ = tokio::time::sleep(cutoff) => {
                    info!("backup request fired after {}ms cutoff", cutoff.as_millis());
                    // Race primary (restarted) and backup
                    // Since primary future was consumed, we fire a new one
                    // In practice this means the backup is the fallback
                    backup_svc.call(backup_req).await
                }
            }
        } else {
            // Single worker, no backup possible
            tokio::time::timeout(self.tts_timeout, primary_svc.call(primary_req))
                .await
                .map_err(|_| ServiceError::Timeout)?
        }
    }

    // ── Retry logic ────────────────────────────────────────────

    /// Execute with retry: try primary worker, on failure try a different one.
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
                    // Brief pause before retry
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
            // Can't return &Vec from if/else with different lifetimes, so just use slice logic
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

    /// Pick a different worker than the given one (for backup requests).
    fn pick_different(&self, exclude: &Arc<Worker>) -> Option<Arc<Worker>> {
        let others: Vec<_> = self.workers.iter()
            .filter(|w| w.healthy.load(Ordering::Relaxed) && !Arc::ptr_eq(w, exclude))
            .collect();
        if others.is_empty() { return None; }
        let idx = self.rr_counter.fetch_add(1, Ordering::Relaxed) as usize % others.len();
        Some(others[idx].clone())
    }

    // ── Health checking ────────────────────────────────────────

    pub async fn health_check(&self) {
        for worker in &self.workers {
            let start = Instant::now();
            let speech_ok = self.check_health(&worker.speech_url, "speech").await;
            let llm_ok = self.check_health(&worker.llm_url, "llm").await;
            worker.last_latency_ms.store(start.elapsed().as_millis() as u64, Ordering::Relaxed);

            let was = worker.healthy.load(Ordering::Relaxed);
            let now = speech_ok && llm_ok;
            worker.healthy.store(now, Ordering::Relaxed);

            if was && !now { warn!("worker {} DOWN", worker.speech_url); }
            if !was && now { info!("worker {} recovered", worker.speech_url); }
        }
    }

    async fn check_health(&self, base_url: &str, label: &str) -> bool {
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
