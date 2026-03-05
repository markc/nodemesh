# Phase 5 — WebRTC SFU via str0m

> Adds audio/video/screen-share forwarding to meshd using str0m, a Sans-I/O Rust WebRTC library. Signaling flows over existing AMP WebSocket connections. WireGuard eliminates NAT traversal complexity.

## Overview

Phase 5 turns meshd from a text-only control plane into a combined control + media plane. A new `sfu` crate embeds str0m's WebRTC stack directly in the meshd binary — no separate media server process. Browsers negotiate WebRTC sessions via AMP signaling messages carried on the existing WebSocket infrastructure, then send/receive RTP media to meshd's UDP socket. meshd forwards media between participants in SFU mode (no transcoding, no mixing).

```
Browser A                    meshd (cachyos)                    Browser B
   │                             │                                  │
   │ AMP: sdp-offer (room.join)  │                                  │
   │ ──────────────────────────► │                                  │
   │                             │ AMP: sdp-offer (room.join)       │
   │                             │ ◄──────────────────────────────── │
   │ AMP: sdp-answer + ice       │                                  │
   │ ◄────────────────────────── │ AMP: sdp-answer + ice            │
   │                             │ ────────────────────────────────► │
   │                             │                                  │
   │     RTP/RTCP over DTLS      │     RTP/RTCP over DTLS           │
   │ ◄═══════════════════════════╪═══════════════════════════════════╡
   │         (UDP)               │         (UDP)                    │
   │                             │                                  │
   │  meshd receives A's media,  │                                  │
   │  forwards to B (and vice    │                                  │
   │  versa). No transcoding.    │                                  │
```

---

## Why str0m

[str0m](https://github.com/algesten/str0m) is a Sans-I/O WebRTC library by Martin Algesten (525 stars). The critical property: **str0m has no async runtime dependency**. You feed it UDP packets and a clock; it tells you what packets to send back. This means:

- **Embeds in meshd's tokio runtime** without a second event loop or runtime conflict
- **Testable without networking** — unit tests feed synthetic packets
- **Full control over UDP socket management** — meshd owns the socket, str0m processes the bytes
- **No hidden threads** — everything runs in meshd's existing task structure

Alternatives considered and rejected:

| Library | Reason to skip |
|---------|---------------|
| webrtc-rs | In transition (maintainers moving on), bundles its own async runtime |
| LiveKit | Separate Go binary — violates the single-binary design |
| mediasoup | C++ compilation dependency, not idiomatic Rust |
| Janus | C codebase, heavyweight, requires plugin architecture |

str0m is purpose-built for embedding in Rust applications. Its SFU use case is the primary design target — the author runs an SFU with it in production.

---

## Crate Structure

A new `sfu` crate joins the workspace:

```
crates/sfu/
├── Cargo.toml          # depends on str0m, amp
└── src/
    ├── lib.rs          # Public API: SfuHandle, start/stop
    ├── room.rs         # Room state, participant tracking
    ├── session.rs      # Per-participant WebRTC session (wraps str0m Rtc)
    ├── media.rs        # Media forwarding logic (SFU routing)
    └── recording.rs    # RTP stream capture for server-side recording
```

### Cargo.toml additions

Workspace root (`Cargo.toml`):

```toml
[workspace]
members = ["crates/meshd", "crates/amp", "crates/sfu"]
resolver = "2"

[workspace.dependencies]
# ... existing deps ...
str0m = "0.7"
```

`crates/sfu/Cargo.toml`:

```toml
[package]
name = "sfu"
version = "0.1.0"
edition = "2021"
description = "WebRTC SFU using str0m — media forwarding for NodeMesh"

[dependencies]
amp = { path = "../amp" }
str0m = { workspace = true }
tokio = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
uuid = { workspace = true }
tracing = { workspace = true }
```

`crates/meshd/Cargo.toml` addition:

```toml
[dependencies]
# ... existing deps ...
sfu = { path = "../sfu" }
```

### Why a separate crate, not a module in meshd

- **Compilation boundary** — str0m and its crypto deps are heavy. Changing meshd code does not recompile SFU code and vice versa.
- **Testability** — the SFU crate can be tested in isolation with synthetic packets, no need for the full meshd stack.
- **Future reuse** — if a standalone SFU binary ever makes sense (dedicated media node), the crate is ready.
- **Matches existing pattern** — `amp` is already a separate crate for the same reasons.

---

## Architecture: How str0m Fits Into meshd

### The event loop

meshd already runs a tokio runtime. str0m integrates via a dedicated task per WebRTC session:

```
meshd main
  ├── peer::manager        (WebSocket connections to other meshd nodes)
  ├── bridge::server       (unix socket for Laravel)
  ├── bridge::callback     (HTTP POST to Laravel)
  ├── server               (WebSocket accept for inbound peers)
  └── sfu::SfuHandle       (NEW)
       ├── UDP socket       (bind 0.0.0.0:9801, RTP/RTCP/DTLS)
       ├── room registry    (HashMap<AmpAddress, Room>)
       └── per-session tasks
            ├── session A   (str0m Rtc instance + forwarding)
            └── session B   (str0m Rtc instance + forwarding)
```

### The str0m Sans-I/O pattern

str0m does not own any sockets. The integration pattern:

```rust
// Pseudocode — the core loop for one WebRTC session
let mut rtc = Rtc::builder().build();

loop {
    // 1. Poll str0m for its next timeout
    let timeout = rtc.poll_output()?.timeout();

    // 2. Wait for either a UDP packet or timeout
    tokio::select! {
        packet = udp_socket.recv_from() => {
            // 3. Feed the packet to str0m
            rtc.handle_input(Input::Receive(now, packet.source, packet.data));
        }
        _ = tokio::time::sleep(timeout) => {
            // 4. Drive timers (DTLS retransmit, RTCP, etc.)
            rtc.handle_input(Input::Timeout(now));
        }
    }

    // 5. Drain outputs — str0m tells us what to send
    while let Ok(output) = rtc.poll_output() {
        match output {
            Output::Transmit(send) => {
                udp_socket.send_to(&send.contents, send.destination).await?;
            }
            Output::Event(event) => {
                // Handle events: track added, track removed, ICE connected, etc.
                handle_event(event);
            }
            Output::Timeout(_) => break,
        }
    }
}
```

This maps cleanly to tokio: `select!` on socket reads and timers, process str0m's outputs synchronously, repeat. No thread spawning, no runtime conflicts.

### UDP socket

meshd binds a single UDP socket for all WebRTC sessions:

```
0.0.0.0:9801/udp   # WebRTC media (RTP/RTCP over DTLS-SRTP)
```

Port 9801 follows meshd's port 9800 (WebSocket). A single socket is shared across all sessions — str0m demultiplexes by source address and DTLS association. This is standard SFU practice (one port, many peers).

Configuration addition to `meshd.toml`:

```toml
[sfu]
enabled = true
udp_bind = "0.0.0.0:9801"
recording_dir = "/var/lib/meshd/recordings"  # optional
max_rooms = 50
max_participants_per_room = 25
```

---

## Signaling Flow Over AMP

WebRTC requires an out-of-band signaling channel to exchange SDP offers/answers and ICE candidates. AMP over WebSocket is that channel. No new transport needed.

### Flow: Browser joins a room

```
1. Browser → Laravel (REST)
   POST /api/rooms/join { room: "standup", media: { audio: true, video: true } }
   Laravel validates auth, creates/finds room, returns room metadata

2. Browser generates SDP offer via RTCPeerConnection

3. Browser → Laravel Reverb (WebSocket)
   Reverb channel: private-mesh.rooms.standup
   Event: sdp-offer
   Payload: { sdp: "...", room: "standup" }

4. Laravel → meshd (unix socket bridge)
   POST /send
   ---
   amp: 1
   type: request
   id: 0192b3a4-5e6f-7890-abcd-ef1234567890
   from: browser.markweb.cachyos.amp
   to: room.chat.cachyos.amp
   command: sdp-offer
   ---
   v=0
   o=- 123456 2 IN IP4 0.0.0.0
   s=-
   t=0 0
   a=group:BUNDLE 0 1
   m=audio 9 UDP/TLS/RTP/SAVPF 111
   ...

5. meshd SFU processes the offer
   - Creates a str0m Rtc instance for this participant
   - Generates SDP answer with meshd's UDP address as ICE candidate
   - Registers participant in room

6. meshd → Laravel (bridge callback)
   POST /api/mesh/inbound
   ---
   amp: 1
   type: response
   id: 0192b3a4-6789-abcd-ef12-345678901234
   from: room.chat.cachyos.amp
   to: browser.markweb.cachyos.amp
   reply-to: 0192b3a4-5e6f-7890-abcd-ef1234567890
   command: sdp-answer
   ---
   v=0
   o=- 789012 2 IN IP4 172.16.2.5
   s=-
   t=0 0
   a=group:BUNDLE 0 1
   m=audio 9801 UDP/TLS/RTP/SAVPF 111
   a=candidate:1 1 udp 2130706431 172.16.2.5 9801 typ host
   ...

7. Laravel → Browser (Reverb WebSocket)
   Forwards SDP answer + ICE candidates to browser

8. Browser completes ICE, DTLS handshake
   RTP media flows: Browser ↔ meshd UDP:9801

9. meshd SFU forwards media
   A's audio/video → B, C, D
   B's audio/video → A, C, D
   (standard SFU fan-out)
```

### AMP signaling commands

| Command | Direction | Body |
|---------|-----------|------|
| `sdp-offer` | browser → meshd | SDP offer text |
| `sdp-answer` | meshd → browser | SDP answer text + ICE candidates |
| `ice-candidate` | either direction | ICE candidate line(s) |
| `room-join` | browser → meshd | Participant metadata |
| `room-leave` | browser → meshd | (empty body) |
| `track-added` | meshd → browser | Track metadata (kind, mid, simulcast layers) |
| `track-removed` | meshd → browser | Track ID |
| `mute` | browser → meshd | `{ "audio": false }` or `{ "video": false }` |

These use the existing AMP message format. No new wire format needed. The SDP bodies are plain text — human-readable and debuggable with `cat`.

### Why signaling goes through Laravel, not direct to meshd

- **Authentication** — Laravel validates session tokens. meshd trusts Laravel.
- **Room lifecycle** — Laravel manages room creation, join permissions, participant limits.
- **Consistency** — all browser communication goes through Reverb. No second WebSocket for signaling.
- **Audit trail** — Laravel can log who joined what room and when.

meshd does not authenticate browsers. It processes signaling messages that arrive from Laravel's bridge. The trust boundary is at the unix socket — only local processes can reach it.

---

## Room Management

### Rooms as AMP addresses

Every room is an AMP address:

```
room.chat.cachyos.amp       # "room" port, "chat" app, "cachyos" node
standup.chat.cachyos.amp    # named room variant
```

The `chat` application on `cachyos` can have many rooms. Each room address routes to the SFU's room registry.

### Room state in the SFU crate

```rust
// crates/sfu/src/room.rs

pub struct Room {
    /// AMP address for this room
    address: AmpAddress,
    /// Active participants
    participants: HashMap<ParticipantId, Participant>,
    /// Creation time
    created_at: Instant,
    /// Optional: recording state
    recording: Option<RecordingState>,
}

pub struct Participant {
    /// Unique ID for this session
    id: ParticipantId,
    /// str0m session handle
    session: SessionHandle,
    /// Published tracks (audio, video, screen)
    published_tracks: Vec<TrackInfo>,
    /// Subscribed tracks from other participants
    subscribed_tracks: Vec<TrackInfo>,
    /// Display name (from Laravel)
    display_name: String,
}
```

### Room lifecycle

| Event | Triggered by | What happens |
|-------|-------------|--------------|
| Create | Laravel REST → meshd bridge | SFU creates `Room`, registers AMP address |
| Join | Browser signaling via Laravel | SFU creates `Participant`, runs str0m Rtc, subscribes to existing tracks |
| Publish track | Browser adds media track | SFU notifies all other participants (track-added event) |
| Leave | Browser disconnects or sends `room-leave` | SFU removes participant, notifies others, stops forwarding |
| Destroy | Laravel REST or last participant leaves | SFU cleans up room state, finalises any recording |

Laravel is authoritative on room policy (who can join, max participants, recording consent). meshd is authoritative on media state (who is sending what, bandwidth, track quality).

---

## WireGuard Simplification

All mesh nodes sit on a WireGuard network with direct IP connectivity. This eliminates the hardest parts of WebRTC deployment.

### What we skip

| Standard WebRTC component | Status | Reason |
|--------------------------|--------|--------|
| STUN server | Not needed | Peers know their WireGuard IPs |
| TURN server | Not needed | Direct connectivity guaranteed within mesh |
| ICE full trickle | Not needed | Single host candidate suffices |
| Symmetric NAT handling | Not needed | WireGuard provides flat network |
| Relay fallback | Not needed | No NAT to traverse |

### ICE configuration

meshd generates ICE candidates using only the WireGuard interface IP:

```
a=candidate:1 1 udp 2130706431 172.16.2.5 9801 typ host
```

One candidate, `typ host`, the WireGuard IP. ICE negotiation completes in a single round-trip because there is nothing to discover — the address is already known and directly reachable.

For browsers not on WireGuard (connecting via the public internet), the server's public IP or a configured STUN server could be added. But within the WireGuard mesh, host candidates are sufficient.

### Triple encryption

Media traffic is encrypted three times:

1. **WireGuard** (network layer) — all traffic between nodes is WireGuard-encrypted regardless of protocol
2. **DTLS-SRTP** (WebRTC mandatory) — WebRTC requires DTLS key exchange for SRTP. This is non-optional per the WebRTC specification.
3. **E2EE via Insertable Streams** (optional) — browser-side end-to-end encryption where even the SFU cannot decrypt media content

For a private mesh, layers 1+2 provide strong confidentiality. Layer 3 is available for scenarios requiring zero-trust media (SFU operator should not see content).

---

## Capabilities

### Audio/video forwarding (SFU mode)

meshd acts as a Selective Forwarding Unit. It receives each participant's encoded media and forwards it to every other participant in the room. No decoding, no transcoding, no mixing.

- **Audio:** Opus codec (browser default). Each participant receives N-1 separate audio streams. Browser-side mixing.
- **Video:** VP8/VP9/AV1 (browser negotiated). Each participant receives N-1 video streams.
- **Bandwidth scales with participants** — SFU upload is O(1) per sender, download is O(N-1). For small rooms (2-10 participants), this is efficient. For large rooms, simulcast handles it.

### Simulcast

str0m supports simulcast — browsers send multiple quality layers of the same video track (e.g., 720p + 360p + 180p). The SFU selects which layer to forward based on:

- Receiver's available bandwidth
- Whether the sender is the active speaker
- Receiver's requested quality (thumbnail vs. full-screen)

```rust
// str0m simulcast API (conceptual)
rtc.set_simulcast_layer(track_id, SimulcastLayer::Mid); // forward 360p
rtc.set_simulcast_layer(track_id, SimulcastLayer::High); // switch to 720p
```

This enables a gallery view where only the active speaker gets high-resolution forwarding while thumbnails get low-resolution — standard SFU behavior.

### Screen sharing

Screen sharing uses the same WebRTC flow with a different media track source:

1. Browser calls `getDisplayMedia()` — user selects screen/window/tab
2. Browser adds the screen track to the existing `RTCPeerConnection`
3. AMP `track-added` event notifies other participants: `{ "kind": "video", "source": "screen" }`
4. SFU forwards the screen track like any other video track

No special SFU logic needed. The browser labels the track as screen share; the frontend renders it differently (full-width, no mirroring).

### Data channels

WebRTC data channels provide reliable or unreliable binary messaging over the same DTLS connection. Use cases:

- **Text chat in-call** — reliable, ordered data channel alongside voice
- **File transfer** — reliable data channel for sending files without base64 encoding
- **Real-time annotations** — unreliable data channel for collaborative drawing/pointer sharing
- **Binary AMP** — potential future: AMP messages over data channels for binary payloads

str0m supports data channels natively. meshd can forward data channel messages between room participants using the same SFU routing logic as media.

### Server-side recording

meshd captures RTP streams for recording by tapping the forwarding pipeline:

```
Incoming RTP from participant A
  ├── Forward to participant B (normal SFU)
  ├── Forward to participant C (normal SFU)
  └── Write to recording buffer (capture)
```

Recording pipeline:

1. **Capture** — SFU writes raw RTP packets to per-track files with timing metadata
2. **Post-process** — when recording stops, spawn FFmpeg to remux RTP into standard containers:
   - Audio: Opus RTP → `.ogg` or `.webm`
   - Video: VP8/VP9 RTP → `.webm` or `.mkv`
   - Combined: mux audio + video + screen tracks into a single `.webm` with layout metadata
3. **Storage** — recording files land in `recording_dir` (configurable), named by room + timestamp
4. **Notification** — meshd sends an AMP event to Laravel when recording is complete:

```
---
amp: 1
type: event
from: room.chat.cachyos.amp
to: recording.markweb.cachyos.amp
command: recording-complete
args: {"room": "standup", "duration_secs": 1847, "tracks": 4}
---
Recording saved: /var/lib/meshd/recordings/standup-2026-03-03T09-00-00.webm
```

Laravel can then move the file to permanent storage, trigger transcription, or index into Brane.

```rust
// crates/sfu/src/recording.rs

pub struct RecordingState {
    room_address: AmpAddress,
    started_at: Instant,
    /// Per-track RTP writers
    track_writers: HashMap<TrackId, RtpWriter>,
    /// Output directory
    output_dir: PathBuf,
}

impl RecordingState {
    /// Called on every forwarded RTP packet
    pub fn capture_packet(&mut self, track_id: TrackId, rtp: &[u8], timestamp: Instant) {
        if let Some(writer) = self.track_writers.get_mut(&track_id) {
            writer.write(rtp, timestamp);
        }
    }

    /// Finalize: close files, spawn FFmpeg remux
    pub async fn finalize(self) -> Result<PathBuf> {
        // Close all track writers
        // Spawn: ffmpeg -i audio.rtp -i video.rtp -c copy output.webm
        // Return path to final file
    }
}
```

---

## MCP Integration

meshd gains an MCP (Model Context Protocol) server via the `rmcp` crate (official MCP Rust SDK). This lets AI agents query and control media infrastructure through standard MCP tool calls.

### Dependencies

```toml
# crates/meshd/Cargo.toml (addition)
rmcp = { version = "0.1", features = ["server", "transport-stdio"] }
```

### Exposed tools

| Tool | Parameters | Returns |
|------|-----------|---------|
| `room_list` | — | Array of active rooms with participant counts |
| `room_status` | `room: string` | Participant list, track details, duration, recording state |
| `recording_start` | `room: string` | Confirmation + recording ID |
| `recording_stop` | `room: string` | File path + duration |
| `call_metrics` | `room?: string` | Bandwidth, packet loss, jitter per participant |
| `participant_kick` | `room: string, participant: string` | Confirmation |
| `room_close` | `room: string` | Confirmation |

### Usage scenario

An AI agent monitoring infrastructure can:

```
User: "How's the standup call going?"

Agent calls: room_status({ room: "standup" })
Returns: {
  participants: ["alice", "bob", "carol"],
  duration_secs: 847,
  recording: true,
  tracks: { audio: 3, video: 2, screen: 1 },
  bandwidth_kbps: { inbound: 4200, outbound: 12600 }
}

Agent: "The standup has 3 participants (Alice, Bob, Carol), running for
14 minutes. Carol is screen sharing. Recording is active. Bandwidth
usage is normal at 12.6 Mbps outbound."
```

### MCP transport

meshd exposes MCP over stdio (for Claude Code integration) and optionally over the unix socket bridge (for Laravel-hosted agents). The bridge approach uses AMP-wrapped MCP:

```
---
amp: 1
type: request
from: agent.markweb.cachyos.amp
to: mcp.meshd.cachyos.amp
command: tool-call
args: {"tool": "room_status", "params": {"room": "standup"}}
---
```

This keeps MCP accessible without requiring a separate stdio process — agents on the mesh can call MCP tools via normal AMP routing.

---

## Configuration

Full `meshd.toml` with SFU section:

```toml
[node]
name = "cachyos"
wg_ip = "172.16.2.5"
listen = "0.0.0.0:9800"

[bridge]
socket = "/run/meshd/meshd.sock"
callback_url = "http://127.0.0.1:8000/api/mesh/inbound"

[peers]
[peers.mko]
wg_ip = "172.16.2.210"
port = 9800

[peers.mmc]
wg_ip = "172.16.2.9"
port = 9800

[sfu]
enabled = true
udp_bind = "0.0.0.0:9801"
recording_dir = "/var/lib/meshd/recordings"
max_rooms = 50
max_participants_per_room = 25

[sfu.ice]
# For WireGuard-only deployment, host candidates suffice
use_stun = false
# Uncomment for public-internet browsers:
# stun_server = "stun:stun.l.google.com:19302"

[log]
level = "info"
```

### Systemd unit update

```ini
[Service]
# ... existing fields ...
# SFU needs UDP port
AmbientCapabilities=CAP_NET_BIND_SERVICE
# Recording storage
StateDirectory=meshd
# /var/lib/meshd/ for recordings
ReadWritePaths=/var/lib/meshd
```

Firewall: allow UDP 9801 on the WireGuard interface (`wg0`). No public-facing UDP port needed if all participants connect through WireGuard.

---

## Browser-Side Integration

### React component structure

```
components/
├── VideoRoom.tsx          # Main room container
├── VideoGrid.tsx          # Gallery/speaker layout
├── VideoTile.tsx          # Single participant video
├── MediaControls.tsx      # Mute, camera, screen share, leave
├── useWebRTC.ts           # Hook: RTCPeerConnection lifecycle
├── useSignaling.ts        # Hook: AMP signaling via Reverb
└── useMediaDevices.ts     # Hook: getUserMedia, device selection
```

### Signaling hook (Reverb + AMP)

```typescript
// useSignaling.ts — connects to Reverb, translates to/from AMP signaling
function useSignaling(roomId: string) {
    const channel = useChannel(`private-mesh.rooms.${roomId}`);

    const sendOffer = (sdp: RTCSessionDescription) => {
        channel.whisper('sdp-offer', { sdp: sdp.sdp });
        // Laravel relays to meshd via bridge
    };

    const onAnswer = (callback: (sdp: string) => void) => {
        channel.listen('.sdp-answer', (e: { sdp: string }) => {
            callback(e.sdp);
        });
    };

    return { sendOffer, onAnswer, ... };
}
```

### WebRTC connection (simplified)

```typescript
// useWebRTC.ts
function useWebRTC(signaling: SignalingHook) {
    const pc = new RTCPeerConnection({
        iceServers: []  // No STUN/TURN on WireGuard mesh
    });

    // Add local tracks
    const stream = await navigator.mediaDevices.getUserMedia({
        audio: true, video: true
    });
    stream.getTracks().forEach(t => pc.addTrack(t, stream));

    // Create offer, send via AMP signaling
    const offer = await pc.createOffer();
    await pc.setLocalDescription(offer);
    signaling.sendOffer(offer);

    // Receive answer from meshd
    signaling.onAnswer(async (sdp) => {
        await pc.setRemoteDescription({ type: 'answer', sdp });
    });

    // Handle remote tracks from SFU
    pc.ontrack = (event) => {
        // Each track is from a different participant
        // meshd labels tracks via SDP (mid → participant mapping)
        addRemoteTrack(event.track, event.streams[0]);
    };
}
```

---

## Cross-Node Rooms

When participants on different nodes join the same room, meshd instances coordinate:

```
Browser (on cachyos)          meshd (cachyos)          meshd (mko)          Browser (on mko)
      │                            │                        │                      │
      │  join room.chat            │                        │  join room.chat      │
      │ ─────────────────────────► │ ◄── AMP: room-sync ──► │ ◄─────────────────── │
      │                            │                        │                      │
      │  RTP media ═══════════════►│ ── forward via WS ───► │═══════════════ RTP ═►│
      │  ◄═══════════════ RTP ═════│ ◄── forward via WS ─── │◄═══════════════ RTP  │
```

Media forwarding between nodes uses one of two strategies:

1. **AMP relay** — small rooms: forward RTP packets as binary WebSocket frames between meshd peers. Adds ~1ms latency (WireGuard hop) but requires no additional connections.

2. **Node-to-node WebRTC** — large rooms: establish a WebRTC connection between meshd instances for media relay. str0m supports server-to-server WebRTC natively. More efficient for sustained multi-track forwarding.

Phase 5a-5b focus on single-node rooms. Cross-node forwarding is Phase 5b+ once single-node SFU is stable.

---

## Phase 5 Milestones

### 5a: str0m integration, single peer audio/video

**Goal:** One browser sends audio+video to meshd, meshd echoes it back (loopback).

**Deliverables:**
- `crates/sfu/` with `str0m` dependency
- UDP socket listener in meshd
- SDP offer/answer via AMP bridge
- ICE with WireGuard host candidates
- DTLS-SRTP handshake completes
- Audio/video loopback (receive, send back to same peer)
- Basic React component: camera preview + loopback playback

**Validates:** str0m integration, signaling flow, DTLS over WireGuard, end-to-end media path.

### 5b: Multi-participant SFU forwarding

**Goal:** Multiple browsers in a room, SFU forwards each sender's media to all other participants.

**Deliverables:**
- Room struct with participant registry
- Media forwarding: receive from A, send to B/C/D
- Track notifications (track-added/track-removed events via AMP)
- Join/leave lifecycle
- Gallery view React component
- Active speaker detection (audio level monitoring)

**Validates:** SFU fan-out, multiple simultaneous str0m sessions, room management, participant lifecycle.

### 5c: Simulcast + bandwidth adaptation

**Goal:** Browsers send multiple quality layers; SFU selects optimal layer per receiver.

**Deliverables:**
- Simulcast negotiation in SDP
- Layer selection logic (active speaker → high, thumbnails → low)
- Bandwidth estimation per receiver
- Dynamic layer switching based on network conditions
- Screen share track support (`getDisplayMedia`)

**Validates:** Quality adaptation, bandwidth management, mixed media types.

### 5d: Recording pipeline

**Goal:** Server-side recording of room sessions.

**Deliverables:**
- RTP stream capture in `recording.rs`
- FFmpeg post-processing (RTP → .webm)
- Start/stop recording via AMP commands from Laravel
- Recording-complete notification to Laravel
- Multi-track layout (grid or active-speaker) for combined recordings

**Validates:** Stream capture, FFmpeg integration, file management.

### 5e: MCP tools

**Goal:** AI agents can query and control media infrastructure.

**Deliverables:**
- `rmcp` integration in meshd
- `room_list`, `room_status`, `call_metrics` tools
- `recording_start`, `recording_stop` tools
- `participant_kick`, `room_close` tools
- Stdio transport for Claude Code
- AMP-wrapped MCP for mesh agents

**Validates:** MCP server in Rust, tool definitions, agent usability.

---

## Risk Assessment

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| str0m API instability (pre-1.0) | Medium | Medium | Pin version, vendor if needed. str0m's author is responsive. |
| DTLS crypto complexity | Low | High | str0m handles DTLS internally. Choose `openssl` backend (well-tested). |
| Single UDP socket scalability | Low | Medium | One socket handles hundreds of concurrent sessions (kernel-level demux). Proven pattern in production SFUs. |
| FFmpeg dependency for recording | Low | Low | FFmpeg is available on all target nodes. Post-processing is async, not in the media path. |
| Browser codec negotiation | Medium | Low | Stick to Opus (audio) + VP8 (video) for maximum compatibility. Add VP9/AV1 later. |
| Cross-node media relay latency | Medium | Medium | WireGuard adds ~1-3ms. For voice, this is imperceptible. Address in 5b+ with measurement. |

---

## What Phase 5 Does NOT Include

- **MCU mode** (server-side mixing/transcoding) — SFU only. No CPU-intensive media processing.
- **Public internet TURN relay** — WireGuard mesh provides direct connectivity. Add TURN only if public-internet browsers are needed later.
- **Phone/PSTN bridge** — no telephony gateway. Voice is browser WebRTC only.
- **End-to-end encryption (E2EE)** — DTLS-SRTP is sufficient for a private mesh. Insertable Streams E2EE is a future enhancement if zero-trust is needed.
- **Mobile native apps** — browser-only. Native iOS/Android WebRTC can be added in a future phase.
- **Transcription** — recording captures raw media. Transcription (Whisper/Deepgram) is a separate service that consumes recordings, not part of the SFU.

---

## Related Documents

- [NodeMesh Daemon Design](~/.gh/markweb/_doc/2026-03-02-nodemesh-daemon-design.md) — meshd architecture, bridge interface, peer management
- [Mesh Architecture](~/.gh/markweb/_doc/2026-03-02-mesh-architecture.md) — overall design philosophy, three-reader principle, evolution path
- [AMP Protocol Specification](https://github.com/markc/appmesh/blob/main/_doc/2026-02-28-amp-protocol-specification.md) — wire format, addressing, header fields
- [WebRTC Chat Research](~/.gh/brane/_doc/2026-03-03-webrtc-chat-research.md) — SFU evaluation (str0m, LiveKit, mediasoup), WireGuard simplification analysis
- [str0m repository](https://github.com/algesten/str0m) — Sans-I/O WebRTC library
- [rmcp repository](https://github.com/modelcontextprotocol/rust-sdk) — Official MCP Rust SDK
