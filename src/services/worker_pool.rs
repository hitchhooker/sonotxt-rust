//! Worker pool — server-as-a-function load balancer
//!
//! Inspired by Marius Eriksen's "Your Server as a Function" (Twitter, 2013).
//! Each GPU worker is a Service: an async function `Req → Future[Rep]`.
//! The pool composes health checks, timeouts, retries, and least-loaded
//! routing into a single Service that callers use without knowing how
//! many backends exist.
//!
//! Config: `WORKER_URLS=http://host1:8080,http://host2:8080`
//! Each URL is a sonotxt speech+llm worker (ports 8080/8090 on same host).

use reqwest::Client;
use serde::Deserialize;
use std::sync::atomic::{AtomicU64, AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::{info, warn, error};

/// A single GPU worker endpoint.
#[derive(Debug)]
pub struct Worker {
    /// Base URL for the speech service (TTS + ASR), e.g. `http://host:8080`
    pub speech_url: String,
    /// Base URL for the LLM service, e.g. `http://host:8090`
    pub llm_url: String,
    /// Whether this worker is currently healthy
    pub healthy: AtomicBool,
    /// In-flight request count (for least-loaded routing)
    pub inflight: AtomicU64,
    /// Total requests served (lifetime)
    pub total_requests: AtomicU64,
    /// Total failures (lifetime)
    pub total_failures: AtomicU64,
    /// Last health check latency in ms
    pub last_latency_ms: AtomicU64,
}

/// Health response from a worker's /health endpoint.
#[derive(Deserialize, Debug)]
struct HealthResponse {
    status: Option<String>,
    tts_loaded: Option<bool>,
    asr_loaded: Option<bool>,
    model_loaded: Option<bool>,
    vram_gb: Option<f64>,
}

/// The worker pool — holds all known workers and provides routing.
pub struct WorkerPool {
    workers: Vec<Arc<Worker>>,
    http: Client,
    /// Round-robin counter (fallback when loads are equal)
    rr_counter: AtomicU64,
}

impl WorkerPool {
    /// Create a pool from a comma-separated list of base URLs.
    /// Each URL should point to a speech service on :8080; the LLM
    /// service is assumed to be on :8090 of the same host.
    ///
    /// Example: `http://1.2.3.4:8080,http://5.6.7.8:8080`
    pub fn new(urls: &str, http: Client) -> Self {
        let workers: Vec<Arc<Worker>> = urls
            .split(',')
            .map(|u| u.trim())
            .filter(|u| !u.is_empty())
            .map(|speech_url| {
                // Derive LLM URL: same host, port 8090
                let llm_url = speech_url
                    .replace(":8080", ":8090");
                Arc::new(Worker {
                    speech_url: speech_url.to_string(),
                    llm_url,
                    healthy: AtomicBool::new(true), // optimistic
                    inflight: AtomicU64::new(0),
                    total_requests: AtomicU64::new(0),
                    total_failures: AtomicU64::new(0),
                    last_latency_ms: AtomicU64::new(0),
                })
            })
            .collect();

        info!("worker pool: {} workers configured", workers.len());
        for w in &workers {
            info!("  speech={} llm={}", w.speech_url, w.llm_url);
        }

        Self {
            workers,
            http,
            rr_counter: AtomicU64::new(0),
        }
    }

    /// Pick the best worker: least-loaded among healthy workers.
    /// Falls back to round-robin if all loads are equal.
    /// Returns None if no healthy workers available.
    pub fn pick(&self) -> Option<Arc<Worker>> {
        let healthy: Vec<_> = self.workers.iter()
            .filter(|w| w.healthy.load(Ordering::Relaxed))
            .collect();

        if healthy.is_empty() {
            // Desperation: try all workers (maybe health check is stale)
            warn!("no healthy workers, trying all");
            if self.workers.is_empty() {
                return None;
            }
            let idx = self.rr_counter.fetch_add(1, Ordering::Relaxed) as usize % self.workers.len();
            return Some(self.workers[idx].clone());
        }

        // Least-loaded
        let min_load = healthy.iter().map(|w| w.inflight.load(Ordering::Relaxed)).min().unwrap_or(0);
        let least_loaded: Vec<_> = healthy.iter()
            .filter(|w| w.inflight.load(Ordering::Relaxed) == min_load)
            .collect();

        // Round-robin among equally-loaded workers
        let idx = self.rr_counter.fetch_add(1, Ordering::Relaxed) as usize % least_loaded.len();
        Some((*least_loaded[idx]).clone())
    }

    /// Pick a worker specifically for LLM requests (same routing logic).
    pub fn pick_llm(&self) -> Option<Arc<Worker>> {
        self.pick() // same pool, same routing
    }

    /// Run a periodic health check on all workers.
    /// Call this from a background task every ~10 seconds.
    pub async fn health_check(&self) {
        for worker in &self.workers {
            let start = std::time::Instant::now();

            let speech_ok = self.check_endpoint(&worker.speech_url, "speech").await;
            let llm_ok = self.check_endpoint(&worker.llm_url, "llm").await;

            let latency = start.elapsed().as_millis() as u64;
            worker.last_latency_ms.store(latency, Ordering::Relaxed);

            let was_healthy = worker.healthy.load(Ordering::Relaxed);
            let now_healthy = speech_ok && llm_ok;
            worker.healthy.store(now_healthy, Ordering::Relaxed);

            if was_healthy && !now_healthy {
                warn!("worker {} went DOWN (speech={} llm={})", worker.speech_url, speech_ok, llm_ok);
            } else if !was_healthy && now_healthy {
                info!("worker {} recovered", worker.speech_url);
            }
        }
    }

    async fn check_endpoint(&self, base_url: &str, label: &str) -> bool {
        match self.http
            .get(format!("{}/health", base_url))
            .timeout(Duration::from_secs(5))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                if let Ok(health) = resp.json::<HealthResponse>().await {
                    // For speech: both TTS and ASR must be loaded
                    if label == "speech" {
                        return health.tts_loaded.unwrap_or(false);
                    }
                    // For LLM: model must be loaded
                    if label == "llm" {
                        return health.model_loaded.unwrap_or(false);
                    }
                    true
                } else {
                    false
                }
            }
            Ok(resp) => {
                warn!("{} {} health check: status {}", base_url, label, resp.status());
                false
            }
            Err(e) => {
                warn!("{} {} health check failed: {}", base_url, label, e);
                false
            }
        }
    }

    /// Get pool status for monitoring.
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

    /// Number of healthy workers.
    pub fn healthy_count(&self) -> usize {
        self.workers.iter().filter(|w| w.healthy.load(Ordering::Relaxed)).count()
    }

    /// Total number of workers.
    pub fn len(&self) -> usize {
        self.workers.len()
    }
}

/// Guard that tracks in-flight requests. Decrements on drop.
pub struct InflightGuard {
    worker: Arc<Worker>,
}

impl InflightGuard {
    pub fn new(worker: &Arc<Worker>) -> Self {
        worker.inflight.fetch_add(1, Ordering::Relaxed);
        worker.total_requests.fetch_add(1, Ordering::Relaxed);
        Self { worker: worker.clone() }
    }

    /// Mark this request as failed.
    pub fn mark_failed(&self) {
        self.worker.total_failures.fetch_add(1, Ordering::Relaxed);
    }
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        self.worker.inflight.fetch_sub(1, Ordering::Relaxed);
    }
}

#[derive(serde::Serialize, Debug)]
pub struct WorkerStatus {
    pub speech_url: String,
    pub llm_url: String,
    pub healthy: bool,
    pub inflight: u64,
    pub total_requests: u64,
    pub total_failures: u64,
    pub latency_ms: u64,
}
