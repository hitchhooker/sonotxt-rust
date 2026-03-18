// P2P session signaling for real-time bilingual voice/chat calls
//
// Flow:
//   1. Creator POST /api/p2p/create → gets session_code (6 chars)
//   2. Joiner opens link sonotxt.com/call/{code}
//   3. Both connect WS /ws/p2p/{code}
//   4. WS relays: SDP offers/answers, ICE candidates, chat messages
//   5. WebRTC P2P established, audio/chat flows direct

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, State,
    },
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use futures::{SinkExt, StreamExt};
use rand::Rng;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::{broadcast, RwLock};
use tracing::info;

use crate::AppState;

// In-memory session store (sessions are ephemeral, no DB needed)
type Sessions = Arc<RwLock<HashMap<String, Session>>>;

struct Session {
    tx: broadcast::Sender<String>,
    created_at: std::time::Instant,
    creator_lang: String,
    peer_count: usize,
}

// Lazy static sessions map attached to app state via extension
// We use a separate static since AppState doesn't need to know about this
static SESSIONS: std::sync::OnceLock<Sessions> = std::sync::OnceLock::new();

fn sessions() -> &'static Sessions {
    SESSIONS.get_or_init(|| Arc::new(RwLock::new(HashMap::new())))
}

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/p2p/create", post(create_session))
        .route("/api/p2p/info/:code", get(session_info))
        .route("/ws/p2p/:code", get(ws_handler))
}

#[derive(Deserialize)]
struct CreateRequest {
    /// Creator's language code (e.g. "en", "ja")
    language: String,
}

#[derive(Serialize)]
struct CreateResponse {
    code: String,
    /// Full shareable URL
    url: String,
}

#[derive(Serialize)]
struct SessionInfo {
    exists: bool,
    creator_lang: Option<String>,
    peer_count: Option<usize>,
}

/// Create a new P2P session, returns a 6-char code
async fn create_session(
    Json(req): Json<CreateRequest>,
) -> Json<CreateResponse> {
    let code = generate_code();
    let (tx, _) = broadcast::channel::<String>(64);

    let mut map = sessions().write().await;
    map.insert(code.clone(), Session {
        tx,
        created_at: std::time::Instant::now(),
        creator_lang: req.language,
        peer_count: 0,
    });

    // Clean up old sessions (> 2 hours)
    map.retain(|_, s| s.created_at.elapsed() < std::time::Duration::from_secs(7200));

    Json(CreateResponse {
        url: format!("/call/{}", code),
        code,
    })
}

/// Check if a session exists
async fn session_info(
    Path(code): Path<String>,
) -> Json<SessionInfo> {
    let map = sessions().read().await;
    match map.get(&code) {
        Some(s) => Json(SessionInfo {
            exists: true,
            creator_lang: Some(s.creator_lang.clone()),
            peer_count: Some(s.peer_count),
        }),
        None => Json(SessionInfo {
            exists: false,
            creator_lang: None,
            peer_count: None,
        }),
    }
}

/// WebSocket handler for P2P signaling
async fn ws_handler(
    ws: WebSocketUpgrade,
    Path(code): Path<String>,
    State(_state): State<Arc<AppState>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_p2p_socket(socket, code))
}

async fn handle_p2p_socket(socket: WebSocket, code: String) {
    let (mut ws_tx, mut ws_rx) = socket.split();

    // Get or check session
    let rx = {
        let mut map = sessions().write().await;
        let session = match map.get_mut(&code) {
            Some(s) => s,
            None => {
                let _ = ws_tx.send(Message::Text(
                    serde_json::json!({"type": "error", "message": "session not found"}).to_string()
                )).await;
                let _ = ws_tx.close().await;
                return;
            }
        };

        if session.peer_count >= 2 {
            let _ = ws_tx.send(Message::Text(
                serde_json::json!({"type": "error", "message": "session full"}).to_string()
            )).await;
            let _ = ws_tx.close().await;
            return;
        }

        session.peer_count += 1;
        let peer_num = session.peer_count;
        info!("P2P peer {} joined session {}", peer_num, code);

        // Notify the peer of their role
        let _ = ws_tx.send(Message::Text(
            serde_json::json!({
                "type": "joined",
                "peer": peer_num,
                "creator_lang": session.creator_lang,
            }).to_string()
        )).await;

        // Notify others that a peer joined
        let _ = session.tx.send(
            serde_json::json!({"type": "peer_joined", "peer": peer_num}).to_string()
        );

        session.tx.subscribe()
    };

    let tx = {
        let map = sessions().read().await;
        match map.get(&code) {
            Some(s) => s.tx.clone(),
            None => return,
        }
    };

    // Relay incoming WS messages to broadcast channel
    let code_clone = code.clone();
    let tx_clone = tx.clone();
    let send_task = tokio::spawn(async move {
        while let Some(Ok(msg)) = ws_rx.next().await {
            match msg {
                Message::Text(text) => {
                    // Relay to all peers in this session
                    let _ = tx_clone.send(text);
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
    });

    // Forward broadcast messages to this peer's WS
    let mut rx = rx;
    let recv_task = tokio::spawn(async move {
        while let Ok(msg) = rx.recv().await {
            if ws_tx.send(Message::Text(msg)).await.is_err() {
                break;
            }
        }
    });

    // Wait for either task to finish
    tokio::select! {
        _ = send_task => {},
        _ = recv_task => {},
    }

    // Peer disconnected — decrement count
    let mut map = sessions().write().await;
    if let Some(session) = map.get_mut(&code_clone) {
        session.peer_count = session.peer_count.saturating_sub(1);
        let _ = session.tx.send(
            serde_json::json!({"type": "peer_left"}).to_string()
        );
        info!("P2P peer left session {}, {} remaining", code_clone, session.peer_count);

        // Clean up empty sessions
        if session.peer_count == 0 {
            map.remove(&code_clone);
        }
    }
}

fn generate_code() -> String {
    let mut rng = rand::thread_rng();
    let chars: Vec<char> = "abcdefghjkmnpqrstuvwxyz23456789".chars().collect();
    (0..6).map(|_| chars[rng.gen_range(0..chars.len())]).collect()
}
