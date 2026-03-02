/// Smoke test: start meshd, connect as a fake peer, exchange AMP messages,
/// verify state via bridge /status endpoint.
///
/// Run with: cargo test -p meshd --test smoke -- --nocapture
use std::time::Duration;
use tokio::time::sleep;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use futures_util::SinkExt;

#[tokio::test]
async fn peer_hello_and_status() {
    let config_dir = tempfile::tempdir().unwrap();
    let socket_path = config_dir.path().join("meshd.sock");
    let config_path = config_dir.path().join("meshd.toml");

    let config = format!(
        r#"
[node]
name = "test-node"
wg_ip = "127.0.0.1"
listen = "127.0.0.1:19800"

[bridge]
socket = "{}"
callback_url = "http://127.0.0.1:19801/api/mesh/inbound"

[peers]

[log]
level = "debug"
"#,
        socket_path.display()
    );
    std::fs::write(&config_path, &config).unwrap();

    // Find the meshd binary (in target/debug/ alongside the test binary)
    let meshd_bin = std::env::current_exe()
        .unwrap()
        .parent().unwrap()
        .parent().unwrap()
        .join("meshd");

    // Start meshd
    let mut child = tokio::process::Command::new(&meshd_bin)
        .arg("--config")
        .arg(config_path.to_str().unwrap())
        .env("RUST_LOG", "meshd=debug")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit()) // show logs in test output
        .spawn()
        .unwrap_or_else(|e| panic!("failed to start meshd at {}: {e}", meshd_bin.display()));

    sleep(Duration::from_millis(500)).await;

    // --- Test 1: Check initial status via curl to unix socket ---
    let status = bridge_status(&socket_path).await;
    println!("[test] Initial status: {status}");
    assert!(status.contains("\"test-node\""), "node name should be test-node");
    assert!(status.contains("\"peers\":[]"), "should have no peers initially");

    // --- Test 2: Connect as fake peer, send hello ---
    let (mut ws, _) = connect_async("ws://127.0.0.1:19800/mesh")
        .await
        .expect("WebSocket connect failed");

    let hello = "---\namp: 1\ntype: event\nfrom: fakepeer.amp\ncommand: hello\nargs: {\"wg_ip\":\"10.0.0.1\"}\n---\n";
    ws.send(Message::Text(hello.into())).await.unwrap();
    println!("[test] Sent hello");
    sleep(Duration::from_millis(300)).await;

    // --- Test 3: Verify peer appeared in status ---
    let status = bridge_status(&socket_path).await;
    println!("[test] Status after hello: {status}");
    assert!(status.contains("fakepeer"), "peer should be registered after hello");
    assert!(status.contains("\"connected\":true"), "peer should be connected");

    // --- Test 4: Send heartbeat (empty AMP) ---
    ws.send(Message::Text("---\n---\n".into())).await.unwrap();
    println!("[test] Sent heartbeat — no crash");

    // --- Test 5: Send a command that triggers callback (will fail, but shouldn't crash) ---
    let ping = "---\namp: 1\ntype: request\nfrom: agent.markweb.fakepeer.amp\nto: agent.markweb.test-node.amp\ncommand: ping\nid: test-001\n---\n";
    ws.send(Message::Text(ping.into())).await.unwrap();
    println!("[test] Sent ping (callback will fail — no Laravel)");
    sleep(Duration::from_millis(500)).await;

    // --- Test 6: Connection still alive after failed callback ---
    let status = bridge_status(&socket_path).await;
    assert!(status.contains("\"connected\":true"), "peer should still be connected after failed callback");
    println!("[test] Peer still connected after failed callback");

    // --- Test 7: Send via bridge /send endpoint ---
    let amp_msg = "---\namp: 1\ntype: request\nfrom: test-node.amp\nto: fakepeer.amp\ncommand: pong\n---\n";
    let send_result = bridge_send(&socket_path, amp_msg).await;
    println!("[test] Bridge /send result: {send_result}");

    // Close WebSocket
    ws.close(None).await.unwrap();
    sleep(Duration::from_millis(200)).await;

    // --- Test 8: Peer disconnected after close ---
    let status = bridge_status(&socket_path).await;
    println!("[test] Status after close: {status}");
    assert!(status.contains("\"connected\":false"), "peer should be disconnected after close");

    // Kill meshd
    child.kill().await.unwrap();
    child.wait().await.unwrap();

    println!("\nAll smoke tests passed!");
}

/// Query bridge /status via unix socket using curl.
async fn bridge_status(socket_path: &std::path::Path) -> String {
    let output = tokio::process::Command::new("curl")
        .args([
            "-s",
            "--unix-socket", socket_path.to_str().unwrap(),
            "http://localhost/status",
        ])
        .output()
        .await
        .expect("curl failed");
    String::from_utf8_lossy(&output.stdout).to_string()
}

/// Send an AMP message via bridge /send endpoint.
async fn bridge_send(socket_path: &std::path::Path, amp_body: &str) -> String {
    let output = tokio::process::Command::new("curl")
        .args([
            "-s", "-w", "%{http_code}",
            "--unix-socket", socket_path.to_str().unwrap(),
            "-X", "POST",
            "-H", "Content-Type: text/x-amp",
            "-d", amp_body,
            "http://localhost/send",
        ])
        .output()
        .await
        .expect("curl failed");
    String::from_utf8_lossy(&output.stdout).to_string()
}
