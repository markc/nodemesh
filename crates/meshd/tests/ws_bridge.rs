/// Integration test: connect to meshd's unix socket as a WebSocket bridge,
/// verify that inbound peer messages arrive over the WS instead of HTTP callback.
///
/// Run with: cargo test -p meshd --test ws_bridge -- --nocapture
use std::time::Duration;
use tokio::time::sleep;
use tokio_tungstenite::tungstenite::Message;

#[tokio::test]
async fn ws_bridge_receives_inbound_messages() {
    let config_dir = tempfile::tempdir().unwrap();
    let socket_path = config_dir.path().join("meshd.sock");
    let config_path = config_dir.path().join("meshd.toml");

    let config = format!(
        r#"
[node]
name = "test-node"
wg_ip = "127.0.0.1"
listen = "127.0.0.1:19830"

[bridge]
socket = "{}"
callback_url = "http://127.0.0.1:19831/api/mesh/inbound"

[peers]

[log]
level = "debug"
"#,
        socket_path.display()
    );
    std::fs::write(&config_path, &config).unwrap();

    let meshd_bin = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("meshd");

    let mut child = tokio::process::Command::new(&meshd_bin)
        .arg("--config")
        .arg(config_path.to_str().unwrap())
        .env("RUST_LOG", "meshd=debug")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .unwrap_or_else(|e| panic!("failed to start meshd at {}: {e}", meshd_bin.display()));

    sleep(Duration::from_millis(500)).await;

    // --- Step 1: Verify status shows ws_bridge: false ---
    let status = bridge_status(&socket_path).await;
    println!("[test] Initial status: {status}");
    assert!(
        status.contains("\"ws_bridge\":false"),
        "WS bridge should not be connected initially"
    );

    // --- Step 2: Connect WS bridge to unix socket ---
    let mut bridge_ws = connect_unix_ws(&socket_path, "/ws").await;
    sleep(Duration::from_millis(200)).await;

    // Verify status shows ws_bridge: true
    let status = bridge_status(&socket_path).await;
    println!("[test] Status with bridge: {status}");
    assert!(
        status.contains("\"ws_bridge\":true"),
        "WS bridge should be connected"
    );

    // --- Step 3: Connect a fake peer and send a message ---
    let (mut peer_ws, _) = tokio_tungstenite::connect_async("ws://127.0.0.1:19830/mesh")
        .await
        .expect("peer connect failed");

    use futures_util::{SinkExt, StreamExt};
    let hello = "---\namp: 1\ntype: event\nfrom: testpeer.amp\ncommand: hello\nargs: {\"wg_ip\":\"10.0.0.1\"}\n---\n";
    peer_ws.send(Message::Text(hello.into())).await.unwrap();
    sleep(Duration::from_millis(200)).await;

    // Send a real message (not empty keepalive)
    let ping = "---\namp: 1\ntype: request\nfrom: agent.testpeer.amp\nto: agent.test-node.amp\ncommand: ping\nid: ws-test-001\n---\n";
    peer_ws.send(Message::Text(ping.into())).await.unwrap();

    // --- Step 4: Read the message from the WS bridge ---
    let msg = tokio::time::timeout(Duration::from_secs(3), bridge_ws.next())
        .await
        .expect("timeout waiting for message on WS bridge");

    let text = match msg {
        Some(Ok(Message::Text(t))) => t.to_string(),
        other => panic!("expected text message on WS bridge, got: {other:?}"),
    };

    println!("[test] WS bridge received: {}", &text[..text.len().min(200)]);
    assert!(text.contains("command: ping"), "should contain the ping command");
    assert!(
        text.contains("x-mesh-peer: testpeer"),
        "should contain peer name header"
    );

    // --- Step 5: Close peer and bridge, verify state ---
    peer_ws.close(None).await.unwrap();
    drop(bridge_ws);
    sleep(Duration::from_millis(500)).await;

    let status = bridge_status(&socket_path).await;
    println!("[test] Status after bridge disconnect: {status}");
    assert!(
        status.contains("\"ws_bridge\":false"),
        "WS bridge should be disconnected"
    );

    child.kill().await.unwrap();
    child.wait().await.unwrap();

    println!("\nWS bridge test passed!");
}

/// Connect to a unix socket and upgrade to WebSocket.
/// Returns the full WebSocket stream (not split).
async fn connect_unix_ws(
    socket_path: &std::path::Path,
    path: &str,
) -> tokio_tungstenite::WebSocketStream<tokio::net::UnixStream> {
    use tokio::net::UnixStream;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    let stream = UnixStream::connect(socket_path)
        .await
        .expect("connect to unix socket");

    let request = format!("ws://localhost{path}")
        .into_client_request()
        .expect("build request");

    let (ws, _) = tokio_tungstenite::client_async(request, stream)
        .await
        .expect("WebSocket handshake over unix socket");

    ws
}

/// Query bridge /status via unix socket using curl.
async fn bridge_status(socket_path: &std::path::Path) -> String {
    let output = tokio::process::Command::new("curl")
        .args([
            "-s",
            "--unix-socket",
            socket_path.to_str().unwrap(),
            "http://localhost/status",
        ])
        .output()
        .await
        .expect("curl failed");
    String::from_utf8_lossy(&output.stdout).to_string()
}
