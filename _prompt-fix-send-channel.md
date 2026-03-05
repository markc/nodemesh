# Bug: outbound send channel closed despite peer showing connected

## Summary

The bridge `/send` endpoint fails with `"route failed: send channel closed for {peer}"` on most peer directions, even though `/status` reports all peers as `connected: true`. The `outbound_tx` channel in PeerState is `None` or dropped for connections that were established via **inbound** WebSocket (i.e., the remote peer connected to us), while it works for connections we initiated **outbound**.

## Evidence

All 4 nodes freshly restarted. `/status` on every node shows all 3 peers as `connected: true`. But `/send` only works in specific directions:

```
cachyos → mko:     ACCEPTED
cachyos → gc:      ACCEPTED
cachyos → mmc:     ACCEPTED

mko → cachyos:     FAILED (send channel closed)
mko → gc:          FAILED (send channel closed)
mko → mmc:         ACCEPTED

gc → mko:          ACCEPTED
gc → cachyos:      FAILED (send channel closed)
gc → mmc:          ACCEPTED

mmc → mko:         FAILED (send channel closed)
mmc → cachyos:     FAILED (send channel closed)
mmc → gc:          FAILED (send channel closed)
```

### Pattern analysis

**cachyos can send to everyone.** cachyos is the only node that initiates outbound connections to all 3 peers (it connects to mko, gc, mmc in its config). The other nodes also initiate connections to their peers, but when both sides try to connect simultaneously, one connection wins and the other is either dropped or becomes the "inbound" connection.

**mmc can only send to mko.** mmc likely only has one successful outbound connection (to mko), and the gc and cachyos connections were established as inbound.

**The bug:** When a peer connects TO us (inbound WebSocket), the `identified inbound peer` code path registers the peer in the HashMap but does NOT set up an `outbound_tx` sender on that connection. Only the `connecting peer` (outbound) code path creates the mpsc channel and stores the sender. So messages can only be sent over connections WE initiated, not connections the remote peer initiated.

## Where to look

1. **`crates/meshd/src/peer/manager.rs`** — `PeerState` struct, `outbound_tx` field. Check the `connect()` method (outbound) vs the inbound connection handler. The outbound path likely does:
   ```rust
   let (tx, rx) = mpsc::channel(32);
   state.outbound_tx = Some(tx);
   // spawn task that reads from rx and writes to WebSocket
   ```
   But the inbound path (when a remote peer's WebSocket upgrade is accepted in `server.rs` and handed to the manager) probably only updates `connected: true` and `last_seen` without creating a send channel.

2. **`crates/meshd/src/peer/connection.rs`** — The connection read/write loop. Check if inbound connections have a write half that's wired up to a channel.

3. **`crates/meshd/src/server.rs`** — The WebSocket upgrade handler that accepts inbound connections and passes them to the peer manager.

## Expected fix

When an inbound peer connection is identified, the manager should:
1. Split the WebSocket into read/write halves
2. Create an mpsc channel for the write half
3. Store the `tx` as `outbound_tx` in the PeerState
4. Spawn a task that forwards from the `rx` to the WebSocket write half

This is the same thing the outbound connection path does — the inbound path just needs the same channel setup.

## Bonus: duplicate connections

Since both sides initiate connections to each other, each peer pair may end up with TWO WebSocket connections (one outbound, one inbound). The manager should either:
- Keep only one (prefer outbound, close inbound duplicate), OR
- Use whichever connected last, but ensure the send channel is always set up

## How to test

After fixing, rebuild and restart meshd on any node, then test:

```bash
# From the node
curl -s -X POST --unix-socket /run/meshd/meshd.sock http://meshd/send \
  -H 'Content-Type: text/x-amp' \
  -d '---
amp: 1
from: markweb.THIS_NODE.amp
to: markweb.OTHER_NODE.amp
command: ping
---
'
# Should return "accepted" for ALL peer directions, not just outbound-initiated ones
```
