use amp::{AmpAddress, AmpMessage};
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::Router;
use sfu::{SfuCommand, SfuHandle};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::net::UnixListener;
use tokio::sync::mpsc;
use tracing::{debug, error, info};

use crate::bridge::ws;
use crate::peer::manager::PeerManager;

/// Shared state for bridge HTTP handlers.
pub struct BridgeState {
    pub peer_manager: Arc<PeerManager>,
    pub sfu_handle: Option<SfuHandle>,
    pub start_time: std::time::Instant,
    pub node_name: String,
    /// Channel to hand off inbound messages to the WS bridge.
    /// When a WS bridge connects, it sends a channel sender here;
    /// the dispatcher routes messages to it instead of HTTP callback.
    pub ws_bridge_tx: mpsc::Sender<mpsc::Sender<(String, AmpMessage)>>,
    /// Whether a WS bridge is currently connected.
    pub ws_bridge_connected: Arc<AtomicBool>,
}

/// Start the unix socket HTTP server for Laravel communication.
pub async fn serve(socket_path: &str, state: Arc<BridgeState>) {
    // Remove stale socket file
    let _ = std::fs::remove_file(socket_path);

    let app = Router::new()
        .route("/send", post(handle_send))
        .route("/status", get(handle_status))
        .route("/ws", get(handle_ws_upgrade))
        .with_state(state);

    let listener = match UnixListener::bind(socket_path) {
        Ok(l) => l,
        Err(e) => {
            error!(path = socket_path, error = %e, "failed to bind unix socket");
            return;
        }
    };

    // Make socket accessible by the web server user
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o660));
    }

    info!(path = socket_path, "bridge listening");

    axum::serve(listener, app)
        .await
        .unwrap_or_else(|e| error!(error = %e, "bridge server error"));
}

/// GET /ws — WebSocket upgrade for persistent bridge to Laravel.
async fn handle_ws_upgrade(
    ws: WebSocketUpgrade,
    State(state): State<Arc<BridgeState>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| async move {
        // Only one bridge at a time
        if state.ws_bridge_connected.load(Ordering::Relaxed) {
            info!("WebSocket bridge rejected — another bridge already connected");
            return;
        }

        // Create a channel for the dispatcher to send inbound messages to us
        let (bridge_inbound_tx, bridge_inbound_rx) = mpsc::channel(1024);

        // Tell the dispatcher to route to us
        if state.ws_bridge_tx.send(bridge_inbound_tx).await.is_err() {
            error!("dispatcher channel closed");
            return;
        }

        state.ws_bridge_connected.store(true, Ordering::Relaxed);

        let ws_state = Arc::new(ws::WsBridgeState {
            peer_manager: state.peer_manager.clone(),
            sfu_handle: state.sfu_handle.clone(),
            node_name: state.node_name.clone(),
        });

        ws::bridge_loop(socket, ws_state, bridge_inbound_rx).await;

        state.ws_bridge_connected.store(false, Ordering::Relaxed);
        info!("WebSocket bridge disconnected, reverting to HTTP callback");
    })
}

/// POST /send — Laravel sends an AMP message to route to a peer.
async fn handle_send(
    State(state): State<Arc<BridgeState>>,
    body: String,
) -> impl IntoResponse {
    let msg = match AmpMessage::parse(&body) {
        Some(m) => m,
        None => {
            return (StatusCode::BAD_REQUEST, "invalid AMP message").into_response();
        }
    };

    let msg_id = msg.get("id").unwrap_or("none").to_string();

    // Check if this is an SFU command targeting the local node
    if let Some(ref sfu_handle) = state.sfu_handle {
        if SfuHandle::is_sfu_command(&msg) {
            if let Some(to) = msg.to_addr() {
                if let Some(addr) = AmpAddress::parse(to) {
                    if addr.is_for_node(&state.node_name) {
                        debug!(command = ?msg.command_name(), to = to, "routing to SFU");
                        let cmd = SfuCommand::SdpOffer {
                            request_id: msg_id.clone(),
                            from_addr: msg.from_addr().unwrap_or("unknown").to_string(),
                            to_addr: to.to_string(),
                            sdp: msg.body.clone(),
                            room: msg.get("room").map(|s| s.to_string()),
                            role: msg.get("role").map(|s| s.to_string()),
                        };
                        return match sfu_handle.send(cmd).await {
                            Ok(()) => (
                                StatusCode::ACCEPTED,
                                [("x-mesh-id", msg_id.as_str())],
                                "accepted (sfu)",
                            )
                                .into_response(),
                            Err(e) => {
                                (StatusCode::INTERNAL_SERVER_ERROR, format!("sfu error: {e}"))
                                    .into_response()
                            }
                        };
                    }
                }
            }
        }
    }

    match state.peer_manager.route(msg).await {
        Ok(()) => (
            StatusCode::ACCEPTED,
            [("x-mesh-id", msg_id.as_str())],
            "accepted",
        )
            .into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, format!("route failed: {e}")).into_response(),
    }
}

/// GET /status — Return daemon and peer status as JSON.
async fn handle_status(State(state): State<Arc<BridgeState>>) -> impl IntoResponse {
    let peers = state.peer_manager.status().await;
    let uptime = state.start_time.elapsed().as_secs();

    let status = serde_json::json!({
        "node": state.node_name,
        "uptime_secs": uptime,
        "peers": peers,
        "sfu_enabled": state.sfu_handle.is_some(),
        "ws_bridge": state.ws_bridge_connected.load(Ordering::Relaxed),
    });

    axum::Json(status).into_response()
}
