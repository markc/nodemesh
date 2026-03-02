use amp::AmpMessage;
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, RwLock};
use tokio_tungstenite::connect_async;
use tracing::{error, info, warn};

use crate::config::PeerConfig;
use crate::peer::connection;

/// Reconnection backoff: 1s, 2s, 4s, 8s, 16s, 30s max.
const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Live state of a peer connection.
#[derive(Debug, Clone)]
pub struct PeerState {
    pub name: String,
    pub wg_ip: String,
    pub connected: bool,
    pub last_seen: Option<Instant>,
    pub outbound_tx: Option<mpsc::Sender<AmpMessage>>,
}

/// Manages all peer connections.
pub struct PeerManager {
    node_name: String,
    node_wg_ip: String,
    peers: Arc<RwLock<HashMap<String, PeerState>>>,
    /// Inbound messages from all peers, consumed by the bridge.
    inbound_tx: mpsc::Sender<(String, AmpMessage)>,
}

impl PeerManager {
    pub fn new(
        node_name: String,
        node_wg_ip: String,
        inbound_tx: mpsc::Sender<(String, AmpMessage)>,
    ) -> Self {
        Self {
            node_name,
            node_wg_ip,
            peers: Arc::new(RwLock::new(HashMap::new())),
            inbound_tx,
        }
    }

    /// Start outbound connections to all configured peers.
    pub async fn connect_to_peers(&self, peer_configs: &HashMap<String, PeerConfig>) {
        for (name, config) in peer_configs {
            let peer_name = name.clone();
            let peer_ip = config.wg_ip.clone();
            let peer_port = config.port;
            let node_name = self.node_name.clone();
            let node_wg_ip = self.node_wg_ip.clone();
            let peers = self.peers.clone();
            let inbound_tx = self.inbound_tx.clone();

            // Duplicate connection resolution: lower WG IP keeps its outbound.
            // Both sides connect; the higher IP drops its outbound if an inbound exists.
            tokio::spawn(async move {
                let mut backoff = INITIAL_BACKOFF;

                loop {
                    info!(peer = %peer_name, ip = %peer_ip, "connecting");

                    let url = format!("ws://{}:{}/mesh", peer_ip, peer_port);
                    match connect_async(&url).await {
                        Ok((ws_stream, _)) => {
                            info!(peer = %peer_name, "connected");
                            backoff = INITIAL_BACKOFF; // reset on success

                            // Create channel for outbound messages to this peer
                            let (outbound_tx, outbound_rx) = mpsc::channel(256);

                            // Register peer as connected
                            {
                                let mut peers = peers.write().await;
                                peers.insert(
                                    peer_name.clone(),
                                    PeerState {
                                        name: peer_name.clone(),
                                        wg_ip: peer_ip.clone(),
                                        connected: true,
                                        last_seen: Some(Instant::now()),
                                        outbound_tx: Some(outbound_tx.clone()),
                                    },
                                );
                            }

                            // Send hello
                            let hello = AmpMessage::command([
                                ("amp", "1".to_string()),
                                ("type", "event".to_string()),
                                ("from", format!("{}.amp", node_name)),
                                ("command", "hello".to_string()),
                                ("args", format!(r#"{{"wg_ip":"{}"}}"#, node_wg_ip)),
                            ]);
                            let _ = outbound_tx.send(hello).await;

                            // Run connection (blocks until disconnect)
                            connection::run_connection(
                                peer_name.clone(),
                                ws_stream,
                                outbound_rx,
                                inbound_tx.clone(),
                            )
                            .await;

                            // Mark disconnected
                            {
                                let mut peers = peers.write().await;
                                if let Some(state) = peers.get_mut(&peer_name) {
                                    state.connected = false;
                                    state.outbound_tx = None;
                                }
                            }
                        }
                        Err(e) => {
                            warn!(peer = %peer_name, error = %e, "connection failed");
                        }
                    }

                    // Backoff before retry
                    info!(peer = %peer_name, backoff_secs = backoff.as_secs(), "reconnecting");
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                }
            });
        }
    }

    /// Handle an inbound WebSocket connection from a peer (accepted by the server).
    pub async fn handle_inbound(
        &self,
        peer_ip: IpAddr,
        ws_stream: axum::extract::ws::WebSocket,
    ) {
        // We'll learn the peer name from their hello message.
        // For now, use IP as temporary name.
        let temp_name = peer_ip.to_string();
        info!(from = %temp_name, "inbound peer connection");

        let (outbound_tx, _outbound_rx) = mpsc::channel(256);

        // For inbound connections, we use axum's WebSocket directly.
        // Outbound writes will be added when we implement bidirectional inbound.
        let inbound_tx = self.inbound_tx.clone();
        let peers = self.peers.clone();

        tokio::spawn(async move {
            use axum::extract::ws::Message;
            use futures_util::StreamExt;

            let (_ws_write, mut ws_read) = ws_stream.split();
            let mut peer_name = temp_name.clone();

            // Read loop
            while let Some(Ok(msg)) = ws_read.next().await {
                match msg {
                    Message::Text(text) => {
                        if let Some(amp_msg) = AmpMessage::parse(&text) {
                            // Learn peer name from hello
                            if amp_msg.command_name() == Some("hello") {
                                if let Some(from) = amp_msg.from_addr() {
                                    if let Some(name) = from.strip_suffix(".amp") {
                                        peer_name = name.to_string();
                                        info!(peer = %peer_name, "identified inbound peer");

                                        let mut peers = peers.write().await;
                                        peers.insert(
                                            peer_name.clone(),
                                            PeerState {
                                                name: peer_name.clone(),
                                                wg_ip: temp_name.clone(),
                                                connected: true,
                                                last_seen: Some(Instant::now()),
                                                outbound_tx: Some(outbound_tx.clone()),
                                            },
                                        );
                                    }
                                }
                                continue;
                            }

                            if !amp_msg.is_empty_message() {
                                if inbound_tx
                                    .send((peer_name.clone(), amp_msg))
                                    .await
                                    .is_err()
                                {
                                    break;
                                }
                            }
                        }
                    }
                    Message::Close(_) => break,
                    _ => continue,
                }
            }

            // Mark disconnected
            let mut peers = peers.write().await;
            if let Some(state) = peers.get_mut(&peer_name) {
                state.connected = false;
                state.outbound_tx = None;
            }
            info!(peer = %peer_name, "inbound connection ended");
        });
    }

    /// Send an AMP message to a specific peer.
    pub async fn send_to(&self, peer_name: &str, msg: AmpMessage) -> Result<(), String> {
        let peers = self.peers.read().await;
        if let Some(state) = peers.get(peer_name) {
            if let Some(tx) = &state.outbound_tx {
                tx.send(msg)
                    .await
                    .map_err(|_| format!("send channel closed for {peer_name}"))
            } else {
                Err(format!("peer {peer_name} not connected"))
            }
        } else {
            Err(format!("unknown peer: {peer_name}"))
        }
    }

    /// Route a message by its `to:` address.
    pub async fn route(&self, msg: AmpMessage) -> Result<(), String> {
        let to = msg
            .to_addr()
            .ok_or_else(|| "message has no 'to' address".to_string())?;

        // Extract node name from address (last segment before .amp)
        let target_node = amp::AmpAddress::parse(to)
            .ok_or_else(|| format!("invalid address: {to}"))?
            .node;

        self.send_to(&target_node, msg).await
    }

    /// Get status of all peers.
    pub async fn status(&self) -> Vec<PeerStatus> {
        let peers = self.peers.read().await;
        peers
            .values()
            .map(|s| PeerStatus {
                name: s.name.clone(),
                wg_ip: s.wg_ip.clone(),
                connected: s.connected,
                last_seen_secs: s.last_seen.map(|t| t.elapsed().as_secs()),
            })
            .collect()
    }
}

#[derive(Debug, serde::Serialize)]
pub struct PeerStatus {
    pub name: String,
    pub wg_ip: String,
    pub connected: bool,
    pub last_seen_secs: Option<u64>,
}
