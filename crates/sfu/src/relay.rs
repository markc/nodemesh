use base64::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use str0m::media::{MediaKind, Pt};
use tracing::{debug, info};

/// A serializable media packet for cross-node relay.
///
/// RTP payload is base64-encoded for safe transport over text WebSocket frames.
/// This adds ~33% overhead but keeps compatibility with the existing AMP text transport.
/// Binary WebSocket frames can be added later as an optimization.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelayPacket {
    /// Room name
    pub room: String,
    /// Media kind: "audio" or "video"
    pub kind: String,
    /// RTP payload type
    pub pt: u8,
    /// RTP timestamp
    pub time: u32,
    /// Wallclock time (ms since epoch)
    pub wallclock_ms: u64,
    /// Base64-encoded RTP payload
    pub data: String,
}

impl RelayPacket {
    /// Create a relay packet from str0m MediaData.
    pub fn from_media_data(
        room: &str,
        kind: MediaKind,
        pt: Pt,
        time: u32,
        data: &[u8],
    ) -> Self {
        let pt: u8 = *pt;
        let wallclock_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        Self {
            room: room.to_string(),
            kind: match kind {
                MediaKind::Audio => "audio".to_string(),
                MediaKind::Video => "video".to_string(),
            },
            pt,
            time,
            wallclock_ms,
            data: BASE64_STANDARD.encode(data),
        }
    }

    /// Decode the base64 payload.
    pub fn decode_data(&self) -> Option<Vec<u8>> {
        BASE64_STANDARD.decode(&self.data).ok()
    }

    /// Parse the media kind.
    pub fn media_kind(&self) -> Option<MediaKind> {
        match self.kind.as_str() {
            "audio" => Some(MediaKind::Audio),
            "video" => Some(MediaKind::Video),
            _ => None,
        }
    }
}

/// Tracks which remote nodes are subscribed to local rooms (outbound relay).
///
/// When node B subscribes to room "demo" on this node, we track it here
/// so we know to forward media data.
pub struct OutboundRelays {
    /// room_name → set of node names subscribed
    pub subscriptions: HashMap<String, HashSet<String>>,
}

impl OutboundRelays {
    pub fn new() -> Self {
        Self {
            subscriptions: HashMap::new(),
        }
    }

    /// Add a remote node as subscriber to a local room.
    pub fn subscribe(&mut self, room: &str, node: &str) {
        self.subscriptions
            .entry(room.to_string())
            .or_default()
            .insert(node.to_string());
        info!(room, node, "outbound relay: node subscribed");
    }

    /// Remove a remote node from a local room.
    pub fn unsubscribe(&mut self, room: &str, node: &str) {
        if let Some(nodes) = self.subscriptions.get_mut(room) {
            nodes.remove(node);
            if nodes.is_empty() {
                self.subscriptions.remove(room);
            }
        }
        debug!(room, node, "outbound relay: node unsubscribed");
    }

    /// Get all nodes subscribed to a room.
    pub fn subscribers(&self, room: &str) -> Option<&HashSet<String>> {
        self.subscriptions.get(room)
    }
}

/// Tracks which remote rooms this node is subscribed to (inbound relay).
///
/// When we have local subscribers for a room whose publisher is on another node,
/// we request relay from that node and receive media here.
pub struct InboundRelays {
    /// room_name → source node name
    pub sources: HashMap<String, String>,
}

impl InboundRelays {
    pub fn new() -> Self {
        Self {
            sources: HashMap::new(),
        }
    }

    /// Register that we're receiving relay for a room from a source node.
    pub fn register(&mut self, room: &str, source_node: &str) {
        self.sources
            .insert(room.to_string(), source_node.to_string());
        info!(room, source_node, "inbound relay: registered");
    }

    /// Remove relay registration.
    pub fn remove(&mut self, room: &str) {
        self.sources.remove(room);
    }

    /// Check if we have a relay source for a room.
    pub fn source_for(&self, room: &str) -> Option<&str> {
        self.sources.get(room).map(|s| s.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relay_packet_roundtrip() {
        let packet = RelayPacket::from_media_data(
            "test-room",
            MediaKind::Audio,
            Pt::new_with_value(111),
            48000,
            &[0x80, 0x6f, 0x00, 0x01],
        );

        let json = serde_json::to_string(&packet).unwrap();
        let parsed: RelayPacket = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.room, "test-room");
        assert_eq!(parsed.kind, "audio");
        assert_eq!(parsed.pt, 111);
        assert_eq!(parsed.time, 48000);
        assert_eq!(parsed.decode_data().unwrap(), &[0x80, 0x6f, 0x00, 0x01]);
    }

    #[test]
    fn outbound_relays_subscribe_unsubscribe() {
        let mut relays = OutboundRelays::new();
        relays.subscribe("demo", "nodeB");
        relays.subscribe("demo", "nodeC");

        assert_eq!(relays.subscribers("demo").unwrap().len(), 2);

        relays.unsubscribe("demo", "nodeB");
        assert_eq!(relays.subscribers("demo").unwrap().len(), 1);

        relays.unsubscribe("demo", "nodeC");
        assert!(relays.subscribers("demo").is_none());
    }

    #[test]
    fn inbound_relays_register_remove() {
        let mut relays = InboundRelays::new();
        relays.register("demo", "nodeA");

        assert_eq!(relays.source_for("demo"), Some("nodeA"));

        relays.remove("demo");
        assert!(relays.source_for("demo").is_none());
    }
}
