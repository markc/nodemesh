pub mod relay;
pub mod room;
pub mod session;

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use amp::AmpMessage;
use relay::{InboundRelays, OutboundRelays, RelayPacket};
use room::{Role, RoomRegistry};
use session::{Session, SessionEvent};
use str0m::net::Receive;
use str0m::Input;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

/// SFU configuration.
#[derive(Debug, Clone)]
pub struct SfuConfig {
    /// UDP bind address for WebRTC media (e.g. "0.0.0.0:9801").
    pub udp_bind: SocketAddr,
}

/// Commands sent to the SFU event loop.
#[derive(Debug)]
pub enum SfuCommand {
    /// Browser wants to establish a WebRTC session.
    SdpOffer {
        request_id: String,
        from_addr: String,
        to_addr: String,
        sdp: String,
        /// Room name (None = legacy loopback).
        room: Option<String>,
        /// Role string: "publisher" or "subscriber".
        role: Option<String>,
    },
    /// Remote node wants to subscribe to a local room's media.
    RelaySubscribe {
        room: String,
        from_node: String,
    },
    /// Remote node unsubscribes from a local room.
    RelayUnsubscribe {
        room: String,
        from_node: String,
    },
    /// Inbound relay media data from a remote node.
    RelayData {
        packet: RelayPacket,
    },
    /// Shut down the SFU.
    Shutdown,
}

/// Events emitted by the SFU for meshd to handle.
#[derive(Debug)]
pub enum SfuEvent {
    /// An AMP message to route back through the bridge (e.g. SDP answer).
    SendMessage(AmpMessage),
    /// Request relay subscription from a remote node's SFU.
    /// meshd should send a `relay-subscribe` AMP command to the target node.
    RequestRelay {
        target_node: String,
        room: String,
    },
    /// Cancel relay subscription.
    CancelRelay {
        target_node: String,
        room: String,
    },
    /// Relay media data to a remote node.
    /// meshd should send a `relay-data` AMP command to the target node.
    RelayData {
        target_node: String,
        packet: RelayPacket,
    },
}

/// Handle to a running SFU task.
#[derive(Clone)]
pub struct SfuHandle {
    cmd_tx: mpsc::Sender<SfuCommand>,
}

/// SFU command names recognized in AMP messages.
const SFU_COMMANDS: &[&str] = &["sdp-offer", "relay-subscribe", "relay-unsubscribe", "relay-data"];

impl SfuHandle {
    /// Start the SFU: bind UDP, spawn event loop, return handle + event receiver.
    pub async fn start(
        config: SfuConfig,
    ) -> Result<(Self, mpsc::Receiver<SfuEvent>), std::io::Error> {
        let socket = UdpSocket::bind(config.udp_bind).await?;
        let local_addr = socket.local_addr()?;

        // Resolve 0.0.0.0 to the actual IP — must match the ICE candidate
        // address used in Session::handle_sdp_offer()
        let candidate_addr = if local_addr.ip().is_unspecified() {
            std::net::SocketAddr::new(
                session::detect_local_ip(),
                local_addr.port(),
            )
        } else {
            local_addr
        };

        info!(bind = %local_addr, candidate = %candidate_addr, "SFU started");

        let (cmd_tx, cmd_rx) = mpsc::channel(256);
        let (evt_tx, evt_rx) = mpsc::channel(256);

        tokio::spawn(sfu_loop(socket, candidate_addr, cmd_rx, evt_tx));

        Ok((Self { cmd_tx }, evt_rx))
    }

    /// Send a command to the SFU.
    pub async fn send(&self, cmd: SfuCommand) -> Result<(), String> {
        self.cmd_tx
            .send(cmd)
            .await
            .map_err(|_| "SFU task gone".to_string())
    }

    /// Check if an AMP message is an SFU command (e.g. sdp-offer).
    pub fn is_sfu_command(msg: &AmpMessage) -> bool {
        msg.command_name()
            .map_or(false, |cmd| SFU_COMMANDS.contains(&cmd))
    }

    /// Parse an AMP message into an SfuCommand, if it's a recognized SFU command.
    pub fn parse_command(msg: &AmpMessage) -> Option<SfuCommand> {
        match msg.command_name()? {
            "sdp-offer" => Some(SfuCommand::SdpOffer {
                request_id: msg.get("id").unwrap_or("unknown").to_string(),
                from_addr: msg.from_addr().unwrap_or("unknown").to_string(),
                to_addr: msg.to_addr().unwrap_or("unknown").to_string(),
                sdp: msg.body.clone(),
                room: msg.get("room").map(|s| s.to_string()),
                role: msg.get("role").map(|s| s.to_string()),
            }),
            "relay-subscribe" => {
                let room = msg.get("room")?.to_string();
                let from_node = msg.get("from-node")
                    .map(|s| s.to_string())
                    .or_else(|| {
                        msg.from_addr().and_then(|a| {
                            amp::AmpAddress::parse(a).map(|addr| addr.node)
                        })
                    })?;
                Some(SfuCommand::RelaySubscribe { room, from_node })
            }
            "relay-unsubscribe" => {
                let room = msg.get("room")?.to_string();
                let from_node = msg.get("from-node")
                    .map(|s| s.to_string())
                    .or_else(|| {
                        msg.from_addr().and_then(|a| {
                            amp::AmpAddress::parse(a).map(|addr| addr.node)
                        })
                    })?;
                Some(SfuCommand::RelayUnsubscribe { room, from_node })
            }
            "relay-data" => {
                let packet: RelayPacket = serde_json::from_str(&msg.body).ok()?;
                Some(SfuCommand::RelayData { packet })
            }
            _ => None,
        }
    }
}

/// The main SFU event loop — sans-I/O str0m driven by tokio.
async fn sfu_loop(
    socket: UdpSocket,
    local_addr: SocketAddr,
    mut cmd_rx: mpsc::Receiver<SfuCommand>,
    evt_tx: mpsc::Sender<SfuEvent>,
) {
    let mut sessions: Vec<Session> = Vec::new();
    let mut rooms = RoomRegistry::new();
    let mut outbound_relays = OutboundRelays::new();
    let mut inbound_relays = InboundRelays::new();
    let mut buf = vec![0u8; 2000]; // MTU-sized buffer for UDP

    info!("SFU event loop running");

    loop {
        // Calculate next timeout from all sessions
        let timeout_duration = compute_timeout(&sessions);

        tokio::select! {
            // Command from meshd (SDP offer, shutdown)
            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(SfuCommand::SdpOffer { request_id, from_addr, to_addr, sdp, room, role }) => {
                        let parsed_role = role.as_deref().and_then(Role::parse);
                        match session::Session::handle_sdp_offer(
                            &request_id, &from_addr, &to_addr, &sdp, local_addr,
                            room.clone(), parsed_role,
                        ) {
                            Ok(result) => {
                                let idx = sessions.len();
                                sessions.push(result.session);
                                if evt_tx.send(SfuEvent::SendMessage(result.answer_msg)).await.is_err() {
                                    warn!("event channel closed, shutting down SFU");
                                    break;
                                }

                                // Register in room if room/role specified
                                if let (Some(ref room_name), Some(r)) = (&room, parsed_role) {
                                    rooms.join(room_name, idx, r);
                                }

                                // Drain initial outputs from the new session
                                drain_and_transmit(&socket, &mut sessions[idx]).await;
                            }
                            Err(e) => {
                                error!(request_id = %request_id, error = %e, "SDP offer handling failed");
                            }
                        }
                    }
                    Some(SfuCommand::RelaySubscribe { room, from_node }) => {
                        outbound_relays.subscribe(&room, &from_node);
                    }
                    Some(SfuCommand::RelayUnsubscribe { room, from_node }) => {
                        outbound_relays.unsubscribe(&room, &from_node);
                    }
                    Some(SfuCommand::RelayData { packet }) => {
                        // Inbound relay: write media to local subscribers
                        handle_relay_data(&packet, &rooms, &mut sessions, &socket).await;
                    }
                    Some(SfuCommand::Shutdown) | None => {
                        info!("SFU shutting down");
                        break;
                    }
                }
            }

            // Incoming UDP packet
            result = socket.recv_from(&mut buf) => {
                match result {
                    Ok((len, source)) => {
                        let now = Instant::now();
                        // Build the receive input
                        let Ok(receive) = Receive::new(
                            str0m::net::Protocol::Udp,
                            source,
                            local_addr,
                            &buf[..len],
                        ) else {
                            continue;
                        };
                        let input = Input::Receive(now, receive);

                        // Demux: find which session accepts this packet
                        let idx = sessions.iter().position(|s| s.accepts(&input));
                        if let Some(i) = idx {
                            if let Err(e) = sessions[i].handle_input(input) {
                                warn!(session = %sessions[i].id, error = %e, "input handling failed");
                            }
                            let (transmits, events, _timeout) = sessions[i].drain_outputs(now);
                            for t in transmits {
                                if let Err(e) = socket.send_to(&t.data, t.dest).await {
                                    debug!(dest = %t.dest, error = %e, "UDP send failed");
                                }
                            }
                            process_events(i, events, &mut sessions, &rooms, &outbound_relays, &evt_tx, &socket).await;
                        } else {
                            debug!(source = %source, len, "no session for UDP packet");
                        }
                    }
                    Err(e) => {
                        warn!(error = %e, "UDP recv error");
                    }
                }
            }

            // Timeout: drive sessions forward
            _ = tokio::time::sleep(timeout_duration) => {
                let now = Instant::now();
                // Collect all events from all sessions first, then process
                let mut all_events: Vec<(usize, Vec<SessionEvent>)> = Vec::new();
                for (i, session) in sessions.iter_mut().enumerate() {
                    if let Err(e) = session.handle_input(Input::Timeout(now)) {
                        warn!(session = %session.id, error = %e, "timeout handling failed");
                    }
                    let (transmits, events, _timeout) = session.drain_outputs(now);
                    for t in transmits {
                        if let Err(e) = socket.send_to(&t.data, t.dest).await {
                            debug!(dest = %t.dest, error = %e, "UDP send failed");
                        }
                    }
                    if !events.is_empty() {
                        all_events.push((i, events));
                    }
                }
                for (source_idx, events) in all_events {
                    process_events(source_idx, events, &mut sessions, &rooms, &outbound_relays, &evt_tx, &socket).await;
                }
            }
        }

        // Clean up dead sessions (manual loop for index adjustment in rooms)
        let mut i = 0;
        while i < sessions.len() {
            if !sessions[i].is_alive() {
                info!(session = %sessions[i].id, "removing dead session");
                sessions.remove(i);
                rooms.remove_session(i);
                // Don't increment i — next session is now at i
            } else {
                i += 1;
            }
        }
    }
}

/// Process session events for room-based fanout.
///
/// For each MediaData event from a publisher, find the room and write media
/// to all local subscribers and relay to remote nodes. For KeyframeRequest
/// events from subscribers, forward to the publisher.
async fn process_events(
    source_idx: usize,
    events: Vec<SessionEvent>,
    sessions: &mut Vec<Session>,
    rooms: &RoomRegistry,
    outbound_relays: &OutboundRelays,
    evt_tx: &mpsc::Sender<SfuEvent>,
    socket: &UdpSocket,
) {
    for event in events {
        match event {
            SessionEvent::MediaData(data) => {
                // Determine the media kind from the source session's mid_by_kind
                let kind = sessions[source_idx]
                    .mid_by_kind
                    .iter()
                    .find(|(_, &mid)| mid == data.mid)
                    .map(|(&k, _)| k);

                let Some(kind) = kind else {
                    debug!(session = %sessions[source_idx].id, mid = ?data.mid, "no kind for mid");
                    continue;
                };

                // Find the room where this session is publisher
                let Some(room) = rooms.find_publisher_room(source_idx) else {
                    continue;
                };

                // Forward to remote relay subscribers
                if let Some(relay_nodes) = outbound_relays.subscribers(&room.name) {
                    let packet = RelayPacket::from_media_data(
                        &room.name,
                        kind,
                        data.params.pt(),
                        data.time.numer() as u32,
                        &data.data,
                    );
                    for node in relay_nodes {
                        let _ = evt_tx.send(SfuEvent::RelayData {
                            target_node: node.clone(),
                            packet: packet.clone(),
                        }).await;
                    }
                }

                // Forward to each local subscriber
                for &sub_idx in &room.subscribers {
                    if sub_idx >= sessions.len() {
                        continue;
                    }
                    let Some(&sub_mid) = sessions[sub_idx].mid_by_kind.get(&kind) else {
                        continue;
                    };
                    let Some(writer) = sessions[sub_idx].rtc.writer(sub_mid) else {
                        continue;
                    };
                    let Some(pt) = writer.match_params(data.params.clone()) else {
                        debug!(sub_idx, "no matching PT for subscriber");
                        continue;
                    };
                    if let Err(e) = writer.write(pt, data.network_time, data.time, data.data.clone()) {
                        debug!(sub_idx, error = %e, "fanout write failed");
                    }

                    // Drain subscriber outputs after writing
                    let now = Instant::now();
                    let (transmits, _events, _timeout) = sessions[sub_idx].drain_outputs(now);
                    for t in transmits {
                        if let Err(e) = socket.send_to(&t.data, t.dest).await {
                            debug!(dest = %t.dest, error = %e, "UDP send failed");
                        }
                    }
                }
            }
            SessionEvent::KeyframeRequest(req) => {
                // Subscriber requests keyframe → forward to publisher in same room
                // Find which room this subscriber is in
                for room in rooms.rooms.values() {
                    if !room.subscribers.contains(&source_idx) {
                        continue;
                    }
                    let Some(pub_idx) = room.publisher else {
                        continue;
                    };
                    if pub_idx >= sessions.len() {
                        continue;
                    }

                    // Determine the kind from the subscriber's mid_by_kind
                    let kind = sessions[source_idx]
                        .mid_by_kind
                        .iter()
                        .find(|(_, &mid)| mid == req.mid)
                        .map(|(&k, _)| k);

                    let Some(kind) = kind else {
                        continue;
                    };
                    let Some(&pub_mid) = sessions[pub_idx].mid_by_kind.get(&kind) else {
                        continue;
                    };

                    if let Some(mut writer) = sessions[pub_idx].rtc.writer(pub_mid) {
                        if let Err(e) = writer.request_keyframe(None, req.kind) {
                            debug!(pub_idx, error = %e, "keyframe forward failed");
                        }
                    }
                    break;
                }
            }
        }
    }
}

/// Handle inbound relay data from a remote node.
///
/// Write the media to all local subscribers in the room.
async fn handle_relay_data(
    packet: &RelayPacket,
    rooms: &RoomRegistry,
    sessions: &mut Vec<Session>,
    socket: &UdpSocket,
) {
    let Some(kind) = packet.media_kind() else {
        debug!(kind = %packet.kind, "unknown relay media kind");
        return;
    };

    let Some(data) = packet.decode_data() else {
        debug!(room = %packet.room, "relay packet decode failed");
        return;
    };

    let Some(room) = rooms.rooms.get(&packet.room) else {
        debug!(room = %packet.room, "relay data for unknown room");
        return;
    };

    // Write to all local subscribers
    for &sub_idx in &room.subscribers {
        if sub_idx >= sessions.len() {
            continue;
        }
        let Some(&sub_mid) = sessions[sub_idx].mid_by_kind.get(&kind) else {
            continue;
        };
        let Some(writer) = sessions[sub_idx].rtc.writer(sub_mid) else {
            continue;
        };

        // Find a matching payload type
        let Some(pt) = writer.payload_params().next().map(|p| p.pt()) else {
            continue;
        };

        let now = Instant::now();
        // Use 90kHz for video, 48kHz for audio — standard RTP clock rates
        let freq = match kind {
            str0m::media::MediaKind::Video => str0m::media::Frequency::NINETY_KHZ,
            str0m::media::MediaKind::Audio => str0m::media::Frequency::FORTY_EIGHT_KHZ,
        };
        let time = str0m::media::MediaTime::new(packet.time as u64, freq);
        if let Err(e) = writer.write(pt, now, time, data.clone()) {
            debug!(sub_idx, error = %e, "relay write to subscriber failed");
        }

        // Drain subscriber outputs after writing
        let (transmits, _events, _timeout) = sessions[sub_idx].drain_outputs(now);
        for t in transmits {
            if let Err(e) = socket.send_to(&t.data, t.dest).await {
                debug!(dest = %t.dest, error = %e, "UDP send failed");
            }
        }
    }
}

/// Drain outputs from a session and transmit UDP packets (for non-room paths).
async fn drain_and_transmit(socket: &UdpSocket, session: &mut Session) {
    let now = Instant::now();
    let (transmits, _events, _timeout) = session.drain_outputs(now);
    for t in transmits {
        if let Err(e) = socket.send_to(&t.data, t.dest).await {
            debug!(dest = %t.dest, error = %e, "UDP send failed");
        }
    }
}

/// Compute the timeout duration — 100ms when sessions are active, 1s otherwise.
fn compute_timeout(sessions: &[Session]) -> Duration {
    if sessions.is_empty() {
        Duration::from_secs(1)
    } else {
        Duration::from_millis(100)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_sfu_command_detects_sdp_offer() {
        let msg = AmpMessage::parse(
            "---\ncommand: sdp-offer\nfrom: browser.cachyos.amp\nto: sfu.cachyos.amp\n---\n",
        )
        .unwrap();
        assert!(SfuHandle::is_sfu_command(&msg));
    }

    #[test]
    fn is_sfu_command_ignores_non_sfu() {
        let msg =
            AmpMessage::parse("---\ncommand: ping\nfrom: a.amp\nto: b.amp\n---\n").unwrap();
        assert!(!SfuHandle::is_sfu_command(&msg));
    }

    #[test]
    fn is_sfu_command_ignores_no_command() {
        let msg = AmpMessage::parse("---\nfrom: a.amp\nto: b.amp\n---\n").unwrap();
        assert!(!SfuHandle::is_sfu_command(&msg));
    }

    #[test]
    fn is_sfu_command_ignores_empty() {
        let msg = AmpMessage::empty();
        assert!(!SfuHandle::is_sfu_command(&msg));
    }
}
