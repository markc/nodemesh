use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::time::Instant;

use amp::AmpMessage;
use str0m::change::SdpOffer;
use str0m::media::{KeyframeRequest, MediaKind, Mid};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc};
use tracing::{debug, info, warn};

use crate::room::Role;

/// Detect the machine's default local IP by UDP-connecting to a public address.
/// This doesn't send any packets — it just lets the OS pick the outgoing interface.
pub fn detect_local_ip() -> IpAddr {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").expect("bind ephemeral");
    socket.connect("8.8.8.8:80").expect("connect probe");
    socket.local_addr().expect("local_addr").ip()
}

/// Events collected during session drain, processed by the event loop for fanout.
pub enum SessionEvent {
    /// Media data from a publisher to forward to subscribers.
    MediaData(str0m::media::MediaData),
    /// Keyframe request from a subscriber to forward to the publisher.
    KeyframeRequest(KeyframeRequest),
}

/// A WebRTC session wrapping a str0m Rtc instance.
///
/// Each session corresponds to one browser peer connection.
pub struct Session {
    pub id: String,
    pub from_addr: String,
    pub rtc: Rtc,
    /// Room name (None = legacy loopback mode).
    pub room: Option<String>,
    /// Role within the room.
    pub role: Option<Role>,
    /// Map from MediaKind to Mid, built from MediaAdded events.
    pub mid_by_kind: HashMap<MediaKind, Mid>,
    alive: bool,
}

/// Result of handling an SDP offer: the new session + an AMP answer message.
pub struct OfferResult {
    pub session: Session,
    pub answer_msg: AmpMessage,
}

/// UDP packets the SFU loop needs to transmit.
pub struct Transmit {
    pub dest: SocketAddr,
    pub data: Vec<u8>,
}

impl Session {
    /// Process an SDP offer and create a new session.
    ///
    /// If room/role are provided, the session participates in room-based fanout.
    /// If None, the session operates in legacy loopback mode.
    pub fn handle_sdp_offer(
        request_id: &str,
        from_addr: &str,
        to_addr: &str,
        sdp_json: &str,
        local_addr: SocketAddr,
        room: Option<String>,
        role: Option<Role>,
    ) -> Result<OfferResult, String> {
        let offer: SdpOffer =
            serde_json::from_str(sdp_json).map_err(|e| format!("invalid SDP offer JSON: {e}"))?;

        let now = Instant::now();
        let mut rtc = Rtc::new(now);

        // Add local ICE candidate — if bound to 0.0.0.0, detect the real IP
        let candidate_addr = if local_addr.ip().is_unspecified() {
            SocketAddr::new(detect_local_ip(), local_addr.port())
        } else {
            local_addr
        };
        let candidate = Candidate::host(candidate_addr, "udp")
            .map_err(|e| format!("invalid candidate: {e}"))?;
        rtc.add_local_candidate(candidate);

        // Accept the offer and generate an answer
        let answer = rtc
            .sdp_api()
            .accept_offer(offer)
            .map_err(|e| format!("accept_offer failed: {e}"))?;

        let answer_json =
            serde_json::to_string(&answer).map_err(|e| format!("serialize answer: {e}"))?;

        // Build AMP response message
        let answer_msg = AmpMessage::new(
            [
                ("amp".to_string(), "1".to_string()),
                ("type".to_string(), "response".to_string()),
                ("command".to_string(), "sdp-answer".to_string()),
                ("from".to_string(), to_addr.to_string()),
                ("to".to_string(), from_addr.to_string()),
                ("reply-to".to_string(), request_id.to_string()),
            ]
            .into_iter()
            .collect(),
            answer_json,
        );

        let session = Session {
            id: request_id.to_string(),
            from_addr: from_addr.to_string(),
            rtc,
            room,
            role,
            mid_by_kind: HashMap::new(),
            alive: true,
        };

        info!(
            session_id = request_id,
            from = from_addr,
            room = ?session.room,
            role = ?session.role,
            "SDP offer accepted, session created"
        );

        Ok(OfferResult {
            session,
            answer_msg,
        })
    }

    /// Check if this session should handle the given UDP input.
    pub fn accepts(&self, input: &Input) -> bool {
        self.rtc.accepts(input)
    }

    /// Feed a network input into this session.
    pub fn handle_input(&mut self, input: Input) -> Result<(), String> {
        self.rtc
            .handle_input(input)
            .map_err(|e| format!("handle_input: {e}"))
    }

    /// Drive the session forward: poll outputs, handle events.
    ///
    /// Returns UDP packets to transmit, collected session events (for room fanout),
    /// and the next timeout deadline.
    pub fn drain_outputs(
        &mut self,
        now: Instant,
    ) -> (Vec<Transmit>, Vec<SessionEvent>, Option<Instant>) {
        let mut transmits = Vec::new();
        let mut events = Vec::new();
        let mut next_timeout = None;

        loop {
            match self.rtc.poll_output() {
                Ok(Output::Timeout(deadline)) => {
                    next_timeout = Some(deadline);
                    // Feed timeout if it's already passed
                    if deadline <= now {
                        if let Err(e) = self.rtc.handle_input(Input::Timeout(now)) {
                            warn!(session = %self.id, error = %e, "timeout handling failed");
                            self.alive = false;
                        }
                        continue;
                    }
                    break;
                }
                Ok(Output::Transmit(t)) => {
                    transmits.push(Transmit {
                        dest: t.destination,
                        data: t.contents.to_vec(),
                    });
                }
                Ok(Output::Event(event)) => {
                    if let Some(session_event) = self.handle_event(event) {
                        events.push(session_event);
                    }
                }
                Err(e) => {
                    warn!(session = %self.id, error = %e, "poll_output error");
                    self.alive = false;
                    break;
                }
            }
        }

        (transmits, events, next_timeout)
    }

    /// Handle a str0m event.
    ///
    /// For room sessions: returns SessionEvent for fanout processing.
    /// For loopback sessions (no room): handles media locally, returns None.
    fn handle_event(&mut self, event: Event) -> Option<SessionEvent> {
        match event {
            Event::IceConnectionStateChange(state) => {
                info!(session = %self.id, ?state, "ICE state changed");
                if state == IceConnectionState::Disconnected {
                    self.alive = false;
                }
                None
            }
            Event::Connected => {
                info!(session = %self.id, "WebRTC connected");
                None
            }
            Event::MediaAdded(media) => {
                debug!(session = %self.id, mid = ?media.mid, kind = ?media.kind, "media added");
                self.mid_by_kind.insert(media.kind, media.mid);
                None
            }
            Event::MediaData(data) => {
                if self.room.is_some() {
                    // Room mode: return event for fanout
                    Some(SessionEvent::MediaData(data))
                } else {
                    // Legacy loopback: echo media back
                    let mid = data.mid;
                    let pt = data.pt;
                    let time = data.time;
                    let payload = data.data.clone();
                    let now = Instant::now();
                    if let Some(writer) = self.rtc.writer(mid) {
                        if let Err(e) = writer.write(pt, now, time, payload) {
                            debug!(session = %self.id, ?mid, error = %e, "loopback write failed");
                        }
                    }
                    None
                }
            }
            Event::KeyframeRequest(req) => {
                if self.room.is_some() {
                    // Room mode: return event for fanout
                    Some(SessionEvent::KeyframeRequest(req))
                } else {
                    // Legacy loopback
                    debug!(session = %self.id, mid = ?req.mid, "keyframe requested");
                    if let Some(mut writer) = self.rtc.writer(req.mid) {
                        if let Err(e) = writer.request_keyframe(None, req.kind) {
                            debug!(session = %self.id, error = %e, "keyframe request failed");
                        }
                    }
                    None
                }
            }
            _ => None,
        }
    }

    /// Whether this session is still alive.
    pub fn is_alive(&self) -> bool {
        self.alive && self.rtc.is_alive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use str0m::media::Direction;

    /// Generate a synthetic SDP offer JSON from a client Rtc with audio+video.
    fn make_client_offer() -> (Rtc, String) {
        let now = Instant::now();
        let mut client = Rtc::new(now);
        client.add_local_candidate(
            Candidate::host("127.0.0.1:5000".parse().unwrap(), "udp").unwrap(),
        );

        let mut change = client.sdp_api();
        change.add_media(MediaKind::Audio, Direction::SendRecv, None, None, None);
        change.add_media(MediaKind::Video, Direction::SendRecv, None, None, None);
        let (offer, _pending) = change.apply().unwrap();
        let offer_json = serde_json::to_string(&offer).unwrap();

        (client, offer_json)
    }

    #[test]
    fn handle_sdp_offer_creates_session_and_answer() {
        let (_client, offer_json) = make_client_offer();
        let local_addr: SocketAddr = "127.0.0.1:9801".parse().unwrap();

        let result = Session::handle_sdp_offer(
            "req-001",
            "browser.cachyos.amp",
            "sfu.cachyos.amp",
            &offer_json,
            local_addr,
            None,
            None,
        )
        .expect("handle_sdp_offer should succeed");

        // Session is alive
        assert!(result.session.is_alive());
        assert_eq!(result.session.id, "req-001");
        assert_eq!(result.session.from_addr, "browser.cachyos.amp");

        // Answer message has correct headers
        let msg = &result.answer_msg;
        assert_eq!(msg.command_name(), Some("sdp-answer"));
        assert_eq!(msg.message_type(), Some("response"));
        assert_eq!(msg.from_addr(), Some("sfu.cachyos.amp"));
        assert_eq!(msg.to_addr(), Some("browser.cachyos.amp"));
        assert_eq!(msg.get("reply-to"), Some("req-001"));

        // Body is valid JSON (SDP answer)
        assert!(!msg.body.is_empty());
        let answer: serde_json::Value = serde_json::from_str(&msg.body)
            .expect("answer body should be valid JSON");
        assert!(answer.is_object());
    }

    #[test]
    fn handle_sdp_offer_with_room() {
        let (_client, offer_json) = make_client_offer();
        let local_addr: SocketAddr = "127.0.0.1:9801".parse().unwrap();

        let result = Session::handle_sdp_offer(
            "req-room-001",
            "browser.cachyos.amp",
            "sfu.cachyos.amp",
            &offer_json,
            local_addr,
            Some("test-room".to_string()),
            Some(Role::Publisher),
        )
        .expect("handle_sdp_offer should succeed");

        assert_eq!(result.session.room, Some("test-room".to_string()));
        assert_eq!(result.session.role, Some(Role::Publisher));
    }

    #[test]
    fn handle_sdp_offer_rejects_invalid_json() {
        let local_addr: SocketAddr = "127.0.0.1:9801".parse().unwrap();
        let result = Session::handle_sdp_offer(
            "req-002",
            "browser.cachyos.amp",
            "sfu.cachyos.amp",
            "not valid json",
            local_addr,
            None,
            None,
        );
        assert!(result.is_err());
    }

    #[test]
    fn session_drains_outputs_after_offer() {
        let (_client, offer_json) = make_client_offer();
        let local_addr: SocketAddr = "127.0.0.1:9801".parse().unwrap();

        let mut result = Session::handle_sdp_offer(
            "req-003",
            "browser.cachyos.amp",
            "sfu.cachyos.amp",
            &offer_json,
            local_addr,
            None,
            None,
        )
        .unwrap();

        let now = Instant::now();
        let (transmits, _events, timeout) = result.session.drain_outputs(now);

        // Should have a timeout (session waiting for ICE)
        assert!(timeout.is_some());
        // Session should still be alive
        assert!(result.session.is_alive());

        // Transmits, if any, should target the client's candidate address
        for t in &transmits {
            assert!(!t.data.is_empty());
        }
    }
}
