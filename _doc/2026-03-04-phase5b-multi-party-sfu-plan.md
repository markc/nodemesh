# Phase 5b: Multi-Party SFU — From Loopback to Forwarding

## Status

Phase 5a complete: browser→meshd→SFU→loopback works end-to-end with mic, camera, and screen share. Confirmed with sfu-test harness on localhost.

## Goal

Replace loopback with real media forwarding: one browser shares screen+audio, other browsers on the same or different mesh nodes receive it.

## Architecture Progression

```
5a (done):  Browser A ──▶ SFU ──loopback──▶ Browser A
5b (next):  Browser A ──▶ SFU ──fanout──▶ Browser B  (same node)
5c:         Browser A ──▶ SFU(node1) ──mesh──▶ SFU(node2) ──▶ Browser B
```

## Key Design Decisions Needed

### 1. Room/Channel Model

The SFU needs to know which sessions should receive which media. Options:

- **Simple room model**: AMP header `room: <name>` in the sdp-offer. All sessions in the same room receive each other's media. Good enough for screencasting (1 sender, N viewers).
- **Pub/sub model**: Sessions declare `publish` or `subscribe` roles. More flexible but more complex.
- **Recommendation**: Start with simple rooms. A room has one publisher (screen sharer) and N subscribers (viewers). Subscribers get SendRecv but only the publisher's media is forwarded.

### 2. Session Registry

Currently `sfu_loop` has `Vec<Session>`. Needs to become:

```rust
struct Room {
    name: String,
    publisher: Option<SessionId>,
    sessions: Vec<Session>,
}
// rooms: HashMap<String, Room>
```

### 3. Media Routing (5b — same node)

Replace loopback in `handle_event`:

```rust
Event::MediaData(data) => {
    // Forward to all OTHER sessions in the same room
    for other in room.sessions.iter_mut() {
        if other.id != self.id {
            if let Some(writer) = other.rtc.writer(mid) {
                writer.write(pt, now, time, payload)?;
            }
        }
    }
}
```

Challenge: the current `Session::handle_event` is a method on `&mut self`, so it can't also write to other sessions. Need to restructure — either collect media events and apply them in the room loop, or change the architecture.

### 4. Cross-Node Forwarding (5c)

For viewers on a different mesh node:

- Publisher's SFU encodes media into AMP messages (binary body, `command: media-data`)
- meshd routes via existing WebSocket mesh to the target node
- Target node's SFU decodes and writes to local viewer sessions

This is where WebRTC data channels or raw RTP forwarding over WireGuard could be more efficient than AMP-over-WebSocket. But AMP-over-WebSocket works first, optimise later.

### 5. Signaling Changes

Browser needs to specify intent:

```
command: sdp-offer
room: my-screencast
role: publisher|subscriber
```

The test harness needs a room name input and publish/subscribe buttons.

## Implementation Order

1. **Room registry** — `HashMap<String, Room>` in sfu_loop, room name from AMP header
2. **Local fanout** — publisher media forwarded to subscriber sessions on same node
3. **Test harness update** — room name field, open two browser tabs (one publish, one subscribe)
4. **Cross-node forwarding** — AMP media messages over the existing mesh WebSocket
5. **Optimisation** — consider WebRTC data channels or direct RTP relay for media

## Files Likely Affected

| File | Change |
|------|--------|
| `crates/sfu/src/lib.rs` | Room registry, modified event loop |
| `crates/sfu/src/session.rs` | Decouple media handling from self-borrow |
| `crates/sfu/src/room.rs` | New: Room struct and media routing logic |
| `tools/sfu-test/index.html` | Room name input, publish/subscribe UI |
| `tools/sfu-test/src/main.rs` | Pass room/role headers in AMP message |

## Open Questions

- Should rooms auto-create on first join and auto-destroy when empty?
- Maximum viewers per room? (str0m handles one peer connection per session, so N viewers = N Rtc instances all being fed the same media)
- Do we need SDP renegotiation when a publisher joins/leaves?
- Should the SFU handle video quality adaptation (SVC/simulcast) or just forward full quality?
