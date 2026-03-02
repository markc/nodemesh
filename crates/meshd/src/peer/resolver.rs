use crate::config::PeerConfig;
use std::collections::HashMap;

/// Resolve peers from configuration.
///
/// Phase 2: static config only.
/// Phase 4: will add DNS SRV lookup for `_mesh._tcp.{node}.amp`.
pub fn resolve_peers(config_peers: &HashMap<String, PeerConfig>) -> HashMap<String, PeerConfig> {
    // For now, just clone the static config.
    // DNS SRV resolution will be added in Phase 4.
    config_peers.clone()
}
