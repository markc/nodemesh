use amp::AmpMessage;
use axum::extract::ws::{Message, WebSocket};
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::peer::manager::PeerManager;
use sfu::{SfuCommand, SfuHandle};

/// State shared with the WebSocket bridge connection.
pub struct WsBridgeState {
    pub peer_manager: Arc<PeerManager>,
    pub sfu_handle: Option<SfuHandle>,
    pub node_name: String,
}

/// Run the bridge WebSocket loop.
///
/// - Inbound: peer AMP messages forwarded to Laravel over this WebSocket
/// - Outbound: Laravel sends AMP messages, routed to peers or SFU
pub async fn bridge_loop(
    socket: WebSocket,
    state: Arc<WsBridgeState>,
    mut inbound_rx: mpsc::Receiver<(String, AmpMessage)>,
) {
    use futures_util::{SinkExt, StreamExt};

    let (mut ws_tx, mut ws_rx) = socket.split();

    info!("WebSocket bridge connected");

    loop {
        tokio::select! {
            // Forward inbound peer messages to Laravel
            msg = inbound_rx.recv() => {
                match msg {
                    Some((peer_name, amp_msg)) => {
                        if amp_msg.is_empty_message() {
                            continue;
                        }
                        let mut forwarded = amp_msg.clone();
                        forwarded.headers.insert(
                            "x-mesh-peer".to_string(),
                            peer_name,
                        );
                        let wire = forwarded.to_wire();
                        if ws_tx.send(Message::Text(wire.into())).await.is_err() {
                            warn!("WebSocket bridge: write failed");
                            break;
                        }
                    }
                    None => {
                        info!("inbound channel closed, bridge shutting down");
                        break;
                    }
                }
            }

            // Receive outbound messages from Laravel
            frame = ws_rx.next() => {
                match frame {
                    Some(Ok(Message::Text(text))) => {
                        let text: &str = &text;
                        if let Some(msg) = AmpMessage::parse(text) {
                            route_outbound(&state, msg).await;
                        } else {
                            warn!("WebSocket bridge: invalid AMP from Laravel");
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        info!("WebSocket bridge disconnected");
                        break;
                    }
                    Some(Ok(_)) => continue,
                    Some(Err(e)) => {
                        warn!(error = %e, "WebSocket bridge read error");
                        break;
                    }
                }
            }
        }
    }

    info!("WebSocket bridge loop ended");
}

/// Route an outbound AMP message from Laravel to peers or SFU.
async fn route_outbound(state: &WsBridgeState, msg: AmpMessage) {
    // Check if it's an SFU command first
    if let Some(ref sfu_handle) = state.sfu_handle {
        if SfuHandle::is_sfu_command(&msg) {
            if let Some(to) = msg.to_addr() {
                if let Some(addr) = amp::AmpAddress::parse(to) {
                    if addr.is_for_node(&state.node_name) {
                        let cmd = SfuCommand::SdpOffer {
                            request_id: msg.get("id").unwrap_or("unknown").to_string(),
                            from_addr: msg.from_addr().unwrap_or("unknown").to_string(),
                            to_addr: to.to_string(),
                            sdp: msg.body.clone(),
                            room: msg.get("room").map(|s| s.to_string()),
                            role: msg.get("role").map(|s| s.to_string()),
                        };
                        if let Err(e) = sfu_handle.send(cmd).await {
                            warn!(error = %e, "WS bridge: SFU command failed");
                        }
                        return;
                    }
                }
            }
        }
    }

    // Route to peer
    if let Err(e) = state.peer_manager.route(msg).await {
        warn!(error = %e, "WS bridge: route failed");
    }
}
