use axum::{routing::get, Json, Router};
use serde::Serialize;

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
    speech: ServiceHealth,
    llm: ServiceHealth,
}

#[derive(Serialize)]
struct ServiceHealth {
    url: String,
    healthy: bool,
}

pub fn health_router(speech_url: String, llm_url: String) -> Router {
    let state = HealthState { speech_url, llm_url };
    Router::new()
        .route("/health", get(health_check))
        .with_state(state)
}

#[derive(Clone)]
struct HealthState {
    speech_url: String,
    llm_url: String,
}

async fn health_check(
    axum::extract::State(state): axum::extract::State<HealthState>,
) -> Json<serde_json::Value> {
    let http = reqwest::Client::new();

    let speech_ok = check_service(&http, &state.speech_url).await;
    let llm_ok = check_service(&http, &state.llm_url).await;

    let overall = if speech_ok && llm_ok { "ok" } else { "degraded" };

    Json(serde_json::json!({
        "status": overall,
        "speech": { "url": state.speech_url, "healthy": speech_ok },
        "llm": { "url": state.llm_url, "healthy": llm_ok },
    }))
}

async fn check_service(http: &reqwest::Client, base_url: &str) -> bool {
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
