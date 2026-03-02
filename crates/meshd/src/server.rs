use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{ConnectInfo, State};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tracing::{error, info};

use crate::peer::manager::PeerManager;

/// Start the WebSocket server that accepts inbound peer connections.
pub async fn serve(listen_addr: &str, peer_manager: Arc<PeerManager>) {
    let app = Router::new()
        .route("/mesh", get(ws_upgrade))
        .with_state(peer_manager);

    let listener = match TcpListener::bind(listen_addr).await {
        Ok(l) => l,
        Err(e) => {
            error!(addr = listen_addr, error = %e, "failed to bind");
            return;
        }
    };

    info!(addr = listen_addr, "mesh WebSocket server listening");

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .unwrap_or_else(|e| error!(error = %e, "mesh server error"));
}

/// WebSocket upgrade handler for `/mesh`.
async fn ws_upgrade(
    ws: WebSocketUpgrade,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(peer_manager): State<Arc<PeerManager>>,
) -> impl IntoResponse {
    info!(from = %addr, "peer WebSocket upgrade");
    ws.on_upgrade(move |socket| async move {
        peer_manager.handle_inbound(addr.ip(), socket).await;
    })
}
