/// Integration test: start meshd with SFU enabled, POST SDP offer via bridge,
/// verify SDP answer is forwarded to callback.
///
/// Run with: cargo test -p meshd --test sfu_signaling -- --nocapture
use std::time::Duration;
use tokio::time::sleep;

#[tokio::test]
async fn sdp_offer_returns_answer_via_callback() {
    let config_dir = tempfile::tempdir().unwrap();
    let socket_path = config_dir.path().join("meshd.sock");
    let config_path = config_dir.path().join("meshd.toml");

    // Start a mock callback server that captures the SDP answer
    let callback_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let callback_port = callback_listener.local_addr().unwrap().port();
    let callback_url = format!("http://127.0.0.1:{callback_port}/api/mesh/inbound");

    // Shared state for capturing callback body
    let (answer_tx, mut answer_rx) = tokio::sync::mpsc::channel::<String>(1);

    // Spawn mock callback server
    tokio::spawn(async move {
        loop {
            let (stream, _) = callback_listener.accept().await.unwrap();
            let answer_tx = answer_tx.clone();
            tokio::spawn(async move {
                let io = hyper_util::rt::TokioIo::new(stream);
                let service = hyper::service::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
                    let answer_tx = answer_tx.clone();
                    async move {
                        use http_body_util::BodyExt;
                        let body = req.into_body().collect().await.unwrap().to_bytes();
                        let body_str = String::from_utf8_lossy(&body).to_string();
                        let _ = answer_tx.send(body_str).await;
                        Ok::<_, hyper::Error>(hyper::Response::new(
                            http_body_util::Full::new(hyper::body::Bytes::from("ok")),
                        ))
                    }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(io, service)
                    .await;
            });
        }
    });

    let config = format!(
        r#"
[node]
name = "test-node"
wg_ip = "127.0.0.1"
listen = "127.0.0.1:19810"

[bridge]
socket = "{}"
callback_url = "{}"

[peers]

[log]
level = "debug"

[sfu]
enabled = true
udp_bind = "127.0.0.1:19811"
"#,
        socket_path.display(),
        callback_url,
    );
    std::fs::write(&config_path, &config).unwrap();

    // Find the meshd binary
    let meshd_bin = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("meshd");

    // Start meshd
    let mut child = tokio::process::Command::new(&meshd_bin)
        .arg("--config")
        .arg(config_path.to_str().unwrap())
        .env("RUST_LOG", "meshd=debug,sfu=debug")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .unwrap_or_else(|e| panic!("failed to start meshd at {}: {e}", meshd_bin.display()));

    sleep(Duration::from_millis(500)).await;

    // --- Generate SDP offer from a client Rtc ---
    let offer_json = {
        use std::time::Instant;
        let now = Instant::now();
        let mut client = str0m::Rtc::new(now);
        client.add_local_candidate(
            str0m::Candidate::host("127.0.0.1:5000".parse().unwrap(), "udp").unwrap(),
        );

        let mut change = client.sdp_api();
        change.add_media(
            str0m::media::MediaKind::Audio,
            str0m::media::Direction::SendRecv,
            None,
            None,
            None,
        );
        let (offer, _pending) = change.apply().unwrap();
        serde_json::to_string(&offer).unwrap()
    };

    // --- POST SDP offer to bridge /send ---
    let amp_msg = format!(
        "---\namp: 1\ntype: request\ncommand: sdp-offer\nfrom: browser.cachyos.amp\nto: sfu.test-node.amp\nid: test-sdp-001\n---\n{}\n",
        offer_json
    );

    let send_result = bridge_send(&socket_path, &amp_msg).await;
    println!("[test] Bridge /send result: {send_result}");
    assert!(
        send_result.contains("accepted (sfu)"),
        "should be routed to SFU, got: {send_result}"
    );

    // --- Wait for SDP answer to arrive at callback ---
    let answer_body = tokio::time::timeout(Duration::from_secs(3), answer_rx.recv())
        .await
        .expect("timeout waiting for SDP answer callback")
        .expect("channel closed");

    println!("[test] Callback received: {}", &answer_body[..answer_body.len().min(200)]);

    // Verify the callback contains an SDP answer AMP message
    assert!(answer_body.contains("sdp-answer"), "callback should contain sdp-answer command");
    assert!(answer_body.contains("reply-to: test-sdp-001"), "callback should reference request id");

    // --- Verify status shows SFU enabled ---
    let status = bridge_status(&socket_path).await;
    println!("[test] Status: {status}");
    assert!(status.contains("\"sfu_enabled\":true"), "SFU should be enabled in status");

    // Kill meshd
    child.kill().await.unwrap();
    child.wait().await.unwrap();

    println!("\nSFU signaling test passed!");
}

/// Integration test: publisher and subscriber both get SDP answers with room/role headers.
#[tokio::test]
async fn room_publish_subscribe_signaling() {
    let config_dir = tempfile::tempdir().unwrap();
    let socket_path = config_dir.path().join("meshd.sock");
    let config_path = config_dir.path().join("meshd.toml");

    // Callback server collects all answers
    let callback_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let callback_port = callback_listener.local_addr().unwrap().port();
    let callback_url = format!("http://127.0.0.1:{callback_port}/api/mesh/inbound");

    let (answer_tx, mut answer_rx) = tokio::sync::mpsc::channel::<String>(8);

    tokio::spawn(async move {
        loop {
            let (stream, _) = callback_listener.accept().await.unwrap();
            let answer_tx = answer_tx.clone();
            tokio::spawn(async move {
                let io = hyper_util::rt::TokioIo::new(stream);
                let service = hyper::service::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
                    let answer_tx = answer_tx.clone();
                    async move {
                        use http_body_util::BodyExt;
                        let body = req.into_body().collect().await.unwrap().to_bytes();
                        let body_str = String::from_utf8_lossy(&body).to_string();
                        let _ = answer_tx.send(body_str).await;
                        Ok::<_, hyper::Error>(hyper::Response::new(
                            http_body_util::Full::new(hyper::body::Bytes::from("ok")),
                        ))
                    }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(io, service)
                    .await;
            });
        }
    });

    let config = format!(
        r#"
[node]
name = "test-node"
wg_ip = "127.0.0.1"
listen = "127.0.0.1:19820"

[bridge]
socket = "{}"
callback_url = "{}"

[peers]

[log]
level = "debug"

[sfu]
enabled = true
udp_bind = "127.0.0.1:19821"
"#,
        socket_path.display(),
        callback_url,
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
        .env("RUST_LOG", "meshd=debug,sfu=debug")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .unwrap_or_else(|e| panic!("failed to start meshd at {}: {e}", meshd_bin.display()));

    sleep(Duration::from_millis(500)).await;

    // --- Publisher SDP offer with room and role ---
    let pub_offer = make_sdp_offer(true);
    let pub_msg = format!(
        "---\namp: 1\ntype: request\ncommand: sdp-offer\nfrom: pub.cachyos.amp\nto: sfu.test-node.amp\nid: pub-001\nroom: demo-room\nrole: publisher\n---\n{}\n",
        pub_offer
    );

    let result = bridge_send(&socket_path, &pub_msg).await;
    assert!(result.contains("accepted (sfu)"), "publisher should be routed to SFU: {result}");

    let pub_answer = tokio::time::timeout(Duration::from_secs(3), answer_rx.recv())
        .await.expect("timeout waiting for publisher answer").expect("channel closed");
    assert!(pub_answer.contains("sdp-answer"), "publisher callback should contain sdp-answer");
    assert!(pub_answer.contains("reply-to: pub-001"), "publisher should reference pub-001");

    // --- Subscriber SDP offer (recvonly) ---
    let sub_offer = make_sdp_offer(false);
    let sub_msg = format!(
        "---\namp: 1\ntype: request\ncommand: sdp-offer\nfrom: sub.cachyos.amp\nto: sfu.test-node.amp\nid: sub-001\nroom: demo-room\nrole: subscriber\n---\n{}\n",
        sub_offer
    );

    let result = bridge_send(&socket_path, &sub_msg).await;
    assert!(result.contains("accepted (sfu)"), "subscriber should be routed to SFU: {result}");

    let sub_answer = tokio::time::timeout(Duration::from_secs(3), answer_rx.recv())
        .await.expect("timeout waiting for subscriber answer").expect("channel closed");
    assert!(sub_answer.contains("sdp-answer"), "subscriber callback should contain sdp-answer");
    assert!(sub_answer.contains("reply-to: sub-001"), "subscriber should reference sub-001");

    child.kill().await.unwrap();
    child.wait().await.unwrap();
    println!("\nRoom signaling test passed!");
}

/// Generate an SDP offer JSON. If send_media is true, uses SendRecv (publisher).
/// If false, uses RecvOnly (subscriber).
fn make_sdp_offer(send_media: bool) -> String {
    use std::time::Instant;
    let now = Instant::now();
    let mut client = str0m::Rtc::new(now);
    client.add_local_candidate(
        str0m::Candidate::host("127.0.0.1:5000".parse().unwrap(), "udp").unwrap(),
    );

    let direction = if send_media {
        str0m::media::Direction::SendRecv
    } else {
        str0m::media::Direction::RecvOnly
    };

    let mut change = client.sdp_api();
    change.add_media(str0m::media::MediaKind::Audio, direction, None, None, None);
    change.add_media(str0m::media::MediaKind::Video, direction, None, None, None);
    let (offer, _pending) = change.apply().unwrap();
    serde_json::to_string(&offer).unwrap()
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

/// Send an AMP message via bridge /send endpoint.
async fn bridge_send(socket_path: &std::path::Path, amp_body: &str) -> String {
    let output = tokio::process::Command::new("curl")
        .args([
            "-s",
            "-w",
            "%{http_code}",
            "--unix-socket",
            socket_path.to_str().unwrap(),
            "-X",
            "POST",
            "-H",
            "Content-Type: text/x-amp",
            "-d",
            amp_body,
            "http://localhost/send",
        ])
        .output()
        .await
        .expect("curl failed");
    String::from_utf8_lossy(&output.stdout).to_string()
}
