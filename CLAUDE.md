# CLAUDE.md — nodemesh

## What This Is

`meshd` — a single Rust daemon that maintains the NodeMesh WebSocket control plane and (future) WebRTC data plane. Runs on every mesh node alongside FrankenPHP/Laravel.

## Architecture

- **amp crate** (`crates/amp/`) — AMP wire format parser/serializer. Pure, no async deps.
- **meshd crate** (`crates/meshd/`) — The daemon binary. tokio + axum + tungstenite.

### Three interfaces

| Interface | Transport | Purpose |
|-----------|----------|---------|
| Peer WebSocket | `ws://{wg_ip}:9800/mesh` | Inter-node AMP messages over WireGuard |
| Bridge unix socket | `/run/meshd/meshd.sock` | Laravel sends outbound AMP, queries status |
| Bridge callback | `POST http://127.0.0.1:8000/api/mesh/inbound` | meshd forwards inbound AMP to Laravel |

## Commands

```bash
cargo build                           # Debug build
cargo build --release                 # Release build
cargo test                            # Run all tests
cargo test -p amp                     # Test AMP crate only
cargo run -- --config config/meshd.example.toml  # Run locally
RUST_LOG=meshd=debug cargo run -- --config config/meshd.example.toml  # Debug logging
```

## Relationship to markweb

meshd replaces markweb's HTTP POST heartbeat/task dispatch system. Laravel talks to meshd via unix socket; meshd maintains persistent WebSocket connections to peer nodes. Reverb stays for browser clients — meshd doesn't touch browsers.

Design doc: `~/.gh/markweb/_doc/2026-03-02-nodemesh-daemon-design.md`
AMP spec: `~/.gh/appmesh/_doc/2026-02-28-amp-protocol-specification.md`

## Phases

- **Phase 2** (current target): WebSocket connections + empty AMP keepalive + bridge
- **Phase 3**: Full AMP routing by `to:` address
- **Phase 4**: DNS SRV peer discovery (replace static config)
- **Phase 5**: WebRTC data channels via `str0m` (binary streaming)
- **Phase 6**: AI agents as mesh participants
