use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, State,
    },
    response::IntoResponse,
    routing::get,
    Router,
};
use futures::{SinkExt, StreamExt};
use std::sync::Arc;
use tokio::time::{interval, Duration};
use tracing::{info, warn};

use crate::{models::JobStatus, AppState};

pub fn routes() -> Router<Arc<AppState>> {
    Router::new().route("/ws/job/:job_id", get(job_status_ws))
}

async fn job_status_ws(
    ws: WebSocketUpgrade,
    Path(job_id): Path<String>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_job_socket(socket, job_id, state))
}

async fn handle_job_socket(socket: WebSocket, job_id: String, state: Arc<AppState>) {
    let (mut sender, mut receiver) = socket.split();

    info!("WebSocket connected for job: {}", job_id);

    // Spawn a task to handle incoming messages (for client pings/close)
    let job_id_clone = job_id.clone();
    tokio::spawn(async move {
        while let Some(msg) = receiver.next().await {
            match msg {
                Ok(Message::Close(_)) => break,
                Ok(Message::Ping(data)) => {
                    // Pong is handled automatically by axum
                    let _ = data;
                }
                Err(e) => {
                    warn!("WebSocket receive error for job {}: {:?}", job_id_clone, e);
                    break;
                }
                _ => {}
            }
        }
    });

    // Send status updates every 500ms until job completes
    let mut tick = interval(Duration::from_millis(500));

    loop {
        tick.tick().await;

        let status = get_job_status(&state, &job_id).await;

        let is_terminal = matches!(
            &status,
            JobStatus::Complete { .. } | JobStatus::Failed { .. }
        );

        let json = match serde_json::to_string(&status) {
            Ok(j) => j,
            Err(e) => {
                warn!("Failed to serialize status: {:?}", e);
                break;
            }
        };

        if sender.send(Message::Text(json)).await.is_err() {
            // Client disconnected
            break;
        }

        if is_terminal {
            info!("Job {} reached terminal state, closing WebSocket", job_id);
            let _ = sender.close().await;
            break;
        }
    }
}

async fn get_job_status(state: &AppState, job_id: &str) -> JobStatus {
    let job = sqlx::query!(
        r#"SELECT
            status,
            audio_url,
            duration_seconds,
            error_message,
            estimated_duration_ms,
            actual_runtime_ms,
            deepinfra_cost,
            started_at,
            created_at,
            storage_type,
            ipfs_cid
        FROM jobs WHERE id = $1"#,
        job_id
    )
    .fetch_optional(&state.db)
    .await;

    let job = match job {
        Ok(Some(j)) => j,
        _ => {
            return JobStatus::Failed {
                reason: "Job not found".into(),
            }
        }
    };

    let estimated_seconds = job.estimated_duration_ms.map(|ms| ms as f64 / 1000.0);

    match job.status.as_str() {
        "completed" => JobStatus::Complete {
            url: job.audio_url.unwrap_or_default(),
            duration_seconds: job.duration_seconds.unwrap_or(0.0),
            runtime_ms: job.actual_runtime_ms,
            cost: job.deepinfra_cost,
            storage_type: job.storage_type,
            ipfs_cid: job.ipfs_cid,
        },
        "failed" => JobStatus::Failed {
            reason: job.error_message.unwrap_or_else(|| "Processing failed".into()),
        },
        "processing" => {
            let elapsed_seconds = job.started_at.map(|started| {
                let now = chrono::Utc::now();
                (now - started).num_milliseconds() as f64 / 1000.0
            });

            let progress: u8 = match (elapsed_seconds, estimated_seconds) {
                (Some(elapsed), Some(estimated)) if estimated > 0.0 => {
                    let pct = ((elapsed / estimated) * 100.0).min(99.0) as u8;
                    pct.max(1)
                }
                _ => 50,
            };

            JobStatus::Processing {
                progress,
                elapsed_seconds,
                estimated_seconds,
            }
        }
        _ => JobStatus::Queued {
            position: None,
            estimated_seconds,
        },
    }
}
