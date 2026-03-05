use amp::AmpMessage;
use tracing::{debug, warn};

/// Forward inbound AMP messages from peers to Laravel via HTTP POST.
pub async fn forward_to_laravel(
    callback_url: &str,
    peer_name: &str,
    msg: &AmpMessage,
) -> Result<(), String> {
    let wire = msg.to_wire();

    debug!(
        peer = peer_name,
        url = callback_url,
        bytes = wire.len(),
        "forwarding to Laravel"
    );

    let client = reqwest::Client::new();
    let response = client
        .post(callback_url)
        .header("content-type", "text/x-amp")
        .header("x-mesh-from", msg.from_addr().unwrap_or("unknown"))
        .header("x-mesh-peer", peer_name)
        .body(wire)
        .send()
        .await
        .map_err(|e| format!("callback POST failed: {e}"))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        warn!(
            peer = peer_name,
            status = %status,
            body = %body,
            "Laravel rejected inbound message"
        );
        return Err(format!("callback returned {status}"));
    }

    debug!(peer = peer_name, "Laravel accepted message");
    Ok(())
}
