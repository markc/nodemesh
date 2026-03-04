pub mod media;
pub mod session;

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use amp::AmpMessage;
use session::Session;
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
    },
    /// Shut down the SFU.
    Shutdown,
}

/// Events emitted by the SFU for meshd to handle.
#[derive(Debug)]
pub enum SfuEvent {
    /// An AMP message to route back through the bridge (e.g. SDP answer).
    SendMessage(AmpMessage),
}

/// Handle to a running SFU task.
#[derive(Clone)]
pub struct SfuHandle {
    cmd_tx: mpsc::Sender<SfuCommand>,
}

/// SFU command names recognized in AMP messages.
const SFU_COMMANDS: &[&str] = &["sdp-offer"];

impl SfuHandle {
    /// Start the SFU: bind UDP, spawn event loop, return handle + event receiver.
    pub async fn start(
        config: SfuConfig,
    ) -> Result<(Self, mpsc::Receiver<SfuEvent>), std::io::Error> {
        let socket = UdpSocket::bind(config.udp_bind).await?;
        let local_addr = socket.local_addr()?;

        info!(bind = %local_addr, "SFU started");

        let (cmd_tx, cmd_rx) = mpsc::channel(256);
        let (evt_tx, evt_rx) = mpsc::channel(256);

        tokio::spawn(sfu_loop(socket, local_addr, cmd_rx, evt_tx));

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
}

/// The main SFU event loop — sans-I/O str0m driven by tokio.
async fn sfu_loop(
    socket: UdpSocket,
    local_addr: SocketAddr,
    mut cmd_rx: mpsc::Receiver<SfuCommand>,
    evt_tx: mpsc::Sender<SfuEvent>,
) {
    let mut sessions: Vec<Session> = Vec::new();
    let mut buf = vec![0u8; 2000]; // MTU-sized buffer for UDP

    info!("SFU event loop running");

    loop {
        // Calculate next timeout from all sessions
        let timeout_duration = compute_timeout(&sessions);

        tokio::select! {
            // Command from meshd (SDP offer, shutdown)
            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(SfuCommand::SdpOffer { request_id, from_addr, to_addr, sdp }) => {
                        match session::Session::handle_sdp_offer(
                            &request_id, &from_addr, &to_addr, &sdp, local_addr,
                        ) {
                            Ok(result) => {
                                sessions.push(result.session);
                                if evt_tx.send(SfuEvent::SendMessage(result.answer_msg)).await.is_err() {
                                    warn!("event channel closed, shutting down SFU");
                                    break;
                                }
                                // Drain initial outputs from the new session
                                drain_and_transmit(&socket, sessions.last_mut().unwrap()).await;
                            }
                            Err(e) => {
                                error!(request_id = %request_id, error = %e, "SDP offer handling failed");
                            }
                        }
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
                            drain_and_transmit(&socket, &mut sessions[i]).await;
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
                for session in &mut sessions {
                    if let Err(e) = session.handle_input(Input::Timeout(now)) {
                        warn!(session = %session.id, error = %e, "timeout handling failed");
                    }
                    drain_and_transmit(&socket, session).await;
                }
            }
        }

        // Clean up dead sessions
        let before = sessions.len();
        sessions.retain(|s| s.is_alive());
        let removed = before - sessions.len();
        if removed > 0 {
            info!(removed, remaining = sessions.len(), "cleaned up dead sessions");
        }
    }
}

/// Drain outputs from a session and transmit UDP packets.
async fn drain_and_transmit(socket: &UdpSocket, session: &mut Session) {
    let now = Instant::now();
    let (transmits, _timeout) = session.drain_outputs(now);
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
