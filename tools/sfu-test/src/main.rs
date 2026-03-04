//! Browser test harness for WebRTC SFU loopback.
//!
//! Bridges browser WebRTC signaling to meshd's unix socket bridge.
//! Run meshd first with config/meshd.sfu-test.toml, then start this.
//!
//! Usage:
//!   Terminal 1: cargo run -p meshd -- --config config/meshd.sfu-test.toml
//!   Terminal 2: cargo run -p sfu-test
//!   Browser:    http://localhost:3000

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse};
use axum::routing::{get, post};
use axum::Router;
use tokio::sync::RwLock;
use tracing::{error, info};

const MESHD_SOCKET: &str = "/tmp/meshd.sock";
const NODE_NAME: &str = "cachyos";
const LISTEN_ADDR: &str = "127.0.0.1:3000";

type PendingAnswers = Arc<RwLock<HashMap<String, String>>>;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "sfu_test=info".into()),
        )
        .init();

    let pending: PendingAnswers = Arc::new(RwLock::new(HashMap::new()));

    let app = Router::new()
        .route("/", get(index_page))
        .route("/signal", post(signal_offer))
        .route("/signal/{id}", get(signal_poll))
        .route("/api/mesh/inbound", post(mesh_callback))
        .with_state(pending);

    let listener = tokio::net::TcpListener::bind(LISTEN_ADDR).await.unwrap();
    info!("SFU test harness listening on http://{LISTEN_ADDR}");
    info!("Make sure meshd is running: cargo run -p meshd -- --config config/meshd.sfu-test.toml");
    axum::serve(listener, app).await.unwrap();
}

/// GET / — serve the test page.
async fn index_page() -> Html<&'static str> {
    Html(include_str!("../index.html"))
}

/// POST /signal — browser sends SDP offer, we forward to meshd via unix socket.
///
/// Expects JSON body: {"type": "offer", "sdp": "v=0\r\n..."}
/// Returns JSON: {"request_id": "..."}
async fn signal_offer(
    State(pending): State<PendingAnswers>,
    body: String,
) -> impl IntoResponse {
    // Validate the browser's SDP offer JSON
    let sdp_value: serde_json::Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(e) => {
            error!("invalid SDP offer JSON: {e}");
            return (StatusCode::BAD_REQUEST, format!("invalid JSON: {e}")).into_response();
        }
    };

    if sdp_value.get("type").and_then(|t| t.as_str()) != Some("offer") {
        return (StatusCode::BAD_REQUEST, "expected type: offer").into_response();
    }

    // Generate a request ID
    let request_id = uuid::Uuid::now_v7().to_string();

    // Pre-register the pending slot so the callback can fill it
    pending.write().await.insert(request_id.clone(), String::new());

    // Wrap in AMP message — str0m's JSON format matches browser format exactly
    let amp_wire = format!(
        "---\namp: 1\ntype: request\ncommand: sdp-offer\nfrom: browser.{NODE_NAME}.amp\nto: sfu.{NODE_NAME}.amp\nid: {request_id}\n---\n{body}\n"
    );

    // Forward to meshd via unix socket
    match send_to_meshd(&amp_wire).await {
        Ok(status) => {
            info!(request_id, "SDP offer forwarded to meshd: {status}");
            let resp = serde_json::json!({"request_id": request_id});
            (StatusCode::OK, axum::Json(resp)).into_response()
        }
        Err(e) => {
            error!("failed to forward to meshd: {e}");
            pending.write().await.remove(&request_id);
            (StatusCode::BAD_GATEWAY, format!("meshd error: {e}")).into_response()
        }
    }
}

/// GET /signal/:id — browser polls for SDP answer.
///
/// Returns 202 if pending, 200 with answer JSON if ready.
async fn signal_poll(
    State(pending): State<PendingAnswers>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let map = pending.read().await;
    match map.get(&id) {
        None => (StatusCode::NOT_FOUND, "unknown request_id").into_response(),
        Some(answer) if answer.is_empty() => {
            (StatusCode::ACCEPTED, "pending").into_response()
        }
        Some(answer) => {
            let answer = answer.clone();
            drop(map);
            // Clean up — answer delivered
            pending.write().await.remove(&id);
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "application/json")],
                answer,
            )
                .into_response()
        }
    }
}

/// POST /api/mesh/inbound — meshd callback with SDP answer AMP message.
async fn mesh_callback(
    State(pending): State<PendingAnswers>,
    body: String,
) -> impl IntoResponse {
    info!("callback received ({} bytes)", body.len());

    // Parse the AMP message
    let msg = match amp::AmpMessage::parse(&body) {
        Some(m) => m,
        None => {
            error!("failed to parse callback AMP message");
            return StatusCode::BAD_REQUEST;
        }
    };

    // Only handle sdp-answer commands
    if msg.command_name() != Some("sdp-answer") {
        info!(command = ?msg.command_name(), "ignoring non-answer callback");
        return StatusCode::OK;
    }

    // Get the request_id from reply-to header
    let request_id = match msg.get("reply-to") {
        Some(id) => id.to_string(),
        None => {
            error!("sdp-answer missing reply-to header");
            return StatusCode::BAD_REQUEST;
        }
    };

    // The body is already {type: "answer", sdp: "..."} JSON from str0m
    let answer_json = msg.body.trim().to_string();

    info!(request_id, "storing SDP answer ({} bytes)", answer_json.len());

    let mut map = pending.write().await;
    if let Some(slot) = map.get_mut(&request_id) {
        *slot = answer_json;
    } else {
        error!(request_id, "no pending slot for answer — request_id not found");
    }

    StatusCode::OK
}

/// Send an AMP message to meshd's unix socket bridge (POST /send).
async fn send_to_meshd(amp_wire: &str) -> Result<String, String> {
    use http_body_util::{BodyExt, Full};
    use hyper::body::Bytes;

    let stream = tokio::net::UnixStream::connect(MESHD_SOCKET)
        .await
        .map_err(|e| format!("connect to {MESHD_SOCKET}: {e}"))?;

    let io = hyper_util::rt::TokioIo::new(stream);
    let (mut sender, conn) = hyper::client::conn::http1::handshake(io)
        .await
        .map_err(|e| format!("handshake: {e}"))?;

    // Drive the connection in the background
    tokio::spawn(async move {
        if let Err(e) = conn.await {
            error!("unix socket connection error: {e}");
        }
    });

    let req = hyper::Request::builder()
        .method("POST")
        .uri("/send")
        .header("content-type", "text/x-amp")
        .body(Full::new(Bytes::from(amp_wire.to_string())))
        .map_err(|e| format!("build request: {e}"))?;

    let resp = sender
        .send_request(req)
        .await
        .map_err(|e| format!("send request: {e}"))?;

    let status = resp.status().to_string();
    let body = resp
        .into_body()
        .collect()
        .await
        .map_err(|e| format!("read body: {e}"))?
        .to_bytes();

    let body_str = String::from_utf8_lossy(&body);
    Ok(format!("{body_str} ({status})"))
}
