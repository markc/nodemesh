/// How media is forwarded within a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForwardingMode {
    /// Echo received media back to the sender (Phase 5a).
    Loopback,
    /// Forward media to all other participants in a room (Phase 5b).
    SfuFanOut,
}
