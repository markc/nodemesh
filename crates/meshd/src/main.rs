mod bridge;
mod config;
mod metrics;
mod peer;
mod server;

use config::Config;
use peer::manager::PeerManager;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{error, info};

#[tokio::main]
async fn main() {
    // Parse args
    let config_path = std::env::args()
        .skip_while(|a| a != "--config")
        .nth(1)
        .unwrap_or_else(|| "/etc/meshd/meshd.toml".to_string());

    let config = Config::load(&PathBuf::from(&config_path)).unwrap_or_else(|e| {
        eprintln!("failed to load config from {config_path}: {e}");
        std::process::exit(1);
    });

    // Init tracing
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(&config.log.level));
    tracing_subscriber::fmt().with_env_filter(env_filter).init();

    info!(
        node = %config.node.name,
        wg_ip = %config.node.wg_ip,
        listen = %config.node.listen,
        peers = config.peers.len(),
        "meshd starting"
    );

    // Channel for inbound messages from all peers → bridge callback
    let (inbound_tx, mut inbound_rx) = mpsc::channel(1024);

    // Peer manager
    let peer_manager = Arc::new(PeerManager::new(
        config.node.name.clone(),
        config.node.wg_ip.clone(),
        inbound_tx,
    ));

    // Connect to configured peers
    let resolved = peer::resolver::resolve_peers(&config.peers);
    peer_manager.connect_to_peers(&resolved).await;

    // Bridge state
    let bridge_state = Arc::new(bridge::server::BridgeState {
        peer_manager: peer_manager.clone(),
        start_time: std::time::Instant::now(),
        node_name: config.node.name.clone(),
    });

    // Spawn bridge unix socket server
    let bridge_socket = config.bridge.socket.clone();
    let bridge_state_clone = bridge_state.clone();
    tokio::spawn(async move {
        bridge::server::serve(&bridge_socket, bridge_state_clone).await;
    });

    // Spawn WebSocket accept server
    let listen_addr = config.node.listen.clone();
    let pm = peer_manager.clone();
    tokio::spawn(async move {
        server::serve(&listen_addr, pm).await;
    });

    // Main loop: forward inbound messages to Laravel
    let callback_url = config.bridge.callback_url.clone();
    info!("meshd ready — forwarding inbound to {callback_url}");

    while let Some((peer_name, msg)) = inbound_rx.recv().await {
        if let Err(e) = bridge::callback::forward_to_laravel(&callback_url, &peer_name, &msg).await
        {
            error!(peer = %peer_name, error = %e, "failed to forward to Laravel");
        }
    }

    error!("inbound channel closed — shutting down");
}
