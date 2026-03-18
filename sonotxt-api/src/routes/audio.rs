use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use std::sync::Arc;

use crate::AppState;

pub fn routes() -> Router<Arc<AppState>> {
    Router::new().route("/audio/*path", get(proxy_audio))
}

async fn proxy_audio(
    State(state): State<Arc<AppState>>,
    Path(path): Path<String>,
) -> Response {
    let minio_url = format!(
        "{}/sonotxt-audio/{}",
        state.config.minio_endpoint,
        path
    );

    match state.http.get(&minio_url).send().await {
        Ok(resp) if resp.status().is_success() => {
            let content_type = resp
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("audio/mpeg")
                .to_string();

            match resp.bytes().await {
                Ok(bytes) => Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", content_type)
                    .header("cache-control", "public, max-age=31536000")
                    .body(axum::body::Body::from(bytes))
                    .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
                Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
            }
        }
        Ok(_) => StatusCode::NOT_FOUND.into_response(),
        Err(_) => StatusCode::BAD_GATEWAY.into_response(),
    }
}
