# WebSocket Bridge: meshd ↔ Laravel

**Date:** 2026-03-04
**Status:** Proposed
**Replaces:** HTTP POST callback bridge (current)

## Problem

The current bridge uses HTTP POST callbacks: meshd POSTs each inbound AMP message to `http://127.0.0.1:8000/api/mesh/inbound` as a separate HTTP request. This works but has drawbacks:

- **Connection overhead** — new TCP connection (or keep-alive reuse) per message
- **No bidirectional streaming** — Laravel can't push messages to meshd without a separate Unix socket call
- **Fragile coupling** — meshd must know Laravel's port; if another app occupies that port, it receives mesh traffic (as discovered with SearchX on :8000)
- **No backpressure** — meshd fires and forgets; if Laravel is slow, messages queue in meshd's tokio runtime

## Current Architecture

```
Peers ←──WebSocket──→ meshd ──HTTP POST──→ Laravel :8000/api/mesh/inbound
                        ↑
Laravel ──HTTP(Unix)──→ meshd /run/meshd/meshd.sock (POST /send, GET /status)
```

Two separate transports, two directions, no shared connection state.

## Proposed Architecture

```
Peers ←──WebSocket──→ meshd ←──WebSocket──→ Laravel
                        ↑
                    Unix socket (GET /status only — kept for health checks)
```

A single persistent WebSocket between meshd and Laravel carries all traffic in both directions:

- **Inbound:** peer AMP messages flow meshd → Laravel (replaces HTTP POST callback)
- **Outbound:** Laravel sends AMP messages to meshd → peers (replaces POST /send on Unix socket)
- **Bidirectional on one connection** — no port guessing, no HTTP overhead

## Design

### meshd side: WebSocket server on Unix socket

meshd already listens on `/run/meshd/meshd.sock` for HTTP. Extend this to accept a WebSocket upgrade at `GET /ws`:

```
/run/meshd/meshd.sock
  GET /status      → JSON health check (unchanged)
  GET /ws          → WebSocket upgrade → persistent bridge
  POST /send       → kept for backward compat / CLI tools (optional)
```

**File:** `crates/meshd/src/bridge/ws.rs` (new)

```rust
/// Accept a WebSocket connection from Laravel on the Unix socket.
/// - Inbound AMP messages from peers are forwarded to Laravel over this WS.
/// - Messages from Laravel are parsed as AMP and routed to the target peer.
pub async fn handle_ws(
    ws: WebSocketUpgrade,
    peer_manager: Arc<PeerManager>,
    inbound_rx: Arc<Mutex<mpsc::Receiver<(String, AmpMessage)>>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| bridge_loop(socket, peer_manager, inbound_rx))
}

async fn bridge_loop(
    socket: WebSocket,
    peer_manager: Arc<PeerManager>,
    inbound_rx: Arc<Mutex<mpsc::Receiver<(String, AmpMessage)>>>,
) {
    let (mut ws_tx, mut ws_rx) = socket.split();

    // Task 1: Forward inbound peer messages → Laravel
    let tx_handle = tokio::spawn(async move {
        let mut rx = inbound_rx.lock().await;
        while let Some((peer_name, msg)) = rx.recv().await {
            if msg.is_empty_message() {
                continue;
            }
            let wire = msg.to_wire();
            // Prepend peer name as a header for Laravel
            if ws_tx.send(Message::Text(wire)).await.is_err() {
                break;
            }
        }
    });

    // Task 2: Laravel → meshd → peer routing
    let rx_handle = tokio::spawn(async move {
        while let Some(Ok(msg)) = ws_rx.next().await {
            if let Message::Text(text) = msg {
                if let Some(amp_msg) = AmpMessage::parse(&text) {
                    if let Err(e) = peer_manager.route(amp_msg).await {
                        warn!(error = %e, "failed to route outbound from Laravel");
                    }
                }
            }
        }
    });

    tokio::select! {
        _ = tx_handle => {},
        _ = rx_handle => {},
    }
}
```

### Laravel side: WebSocket client

Laravel connects to meshd's Unix socket WebSocket on boot (via a long-running process or a Reverb-style daemon). Options:

#### Option A: Artisan command (recommended)

```php
// app/Console/Commands/MeshBridgeCommand.php
class MeshBridgeCommand extends Command
{
    protected $signature = 'mesh:bridge';
    protected $description = 'Persistent WebSocket bridge to meshd';

    public function handle(): void
    {
        $socket = new WebSocketClient('unix:///run/meshd/meshd.sock/ws');

        // Send outbound messages (from Redis queue or similar)
        $this->sendLoop($socket);

        // Receive inbound messages
        $socket->onMessage(function (string $wire) {
            $msg = AmpMessage::parse($wire);
            // Dispatch to Laravel event/job system
            MeshInboundMessage::dispatch($msg);
        });

        $socket->run(); // blocks forever
    }
}
```

Run as: `php artisan mesh:bridge` (supervised by systemd or `composer dev`).

#### Option B: Ratchet/ReactPHP in-process

Use `ratchet/pawl` (ReactPHP WebSocket client) inside a long-running artisan command. Same concept, different WS library.

#### Option C: Reverb integration

Since markweb already runs Laravel Reverb (Pusher-protocol WebSocket server for browsers), the mesh bridge could publish inbound AMP messages directly to Reverb channels. This collapses the mesh → Laravel → Reverb → browser chain into mesh → Reverb → browser for events that are purely pass-through.

This is more complex and can be Phase 2 of the WS bridge work.

### Message framing

Each WebSocket text frame contains exactly one AMP wire message. No batching, no framing protocol beyond WebSocket's own frames. This keeps it simple and compatible with the existing `AmpMessage::parse()` / `to_wire()`.

### Connection lifecycle

1. Laravel connects to `unix:///run/meshd/meshd.sock` and sends HTTP upgrade to `/ws`
2. meshd accepts, starts `bridge_loop`
3. If Laravel disconnects, meshd logs a warning and waits for reconnection
4. If meshd restarts, Laravel reconnects with backoff (1s, 2s, 4s, max 30s)
5. meshd only accepts **one** bridge WebSocket at a time (multiple Laravel processes would be a misconfiguration)

### Backward compatibility

- `POST /send` on the Unix socket can remain for CLI tools (`curl --unix-socket`) and one-off scripts
- `GET /status` remains unchanged
- The HTTP POST callback (`callback_url` in config) becomes optional — if a WebSocket bridge is connected, use it; otherwise fall back to HTTP POST
- Config addition:

```toml
[bridge]
socket = "/run/meshd/meshd.sock"
callback_url = "http://127.0.0.1:8000/api/mesh/inbound"  # fallback only
prefer_ws = true  # use WS bridge when connected, HTTP callback as fallback
```

## Migration Path

1. **Phase 1 (done):** Filter keepalives in HTTP callback — reduces noise now
2. **Phase 2:** Add `GET /ws` endpoint to meshd Unix socket server
3. **Phase 3:** Build `php artisan mesh:bridge` command in markweb
4. **Phase 4:** Wire outbound sends through the WS bridge (replace `MeshBridgeService` curl calls)
5. **Phase 5:** Make HTTP callback optional — WS bridge is primary, HTTP is fallback

## Benefits

- **Single connection, bidirectional** — no port assumptions, no HTTP overhead
- **Instant delivery** — no connection setup per message
- **Backpressure** — WebSocket flow control prevents message flooding
- **Simpler topology** — one fewer transport to reason about
- **Laravel process awareness** — meshd knows if Laravel is connected (WS open) vs disconnected (WS closed), can buffer or drop accordingly
