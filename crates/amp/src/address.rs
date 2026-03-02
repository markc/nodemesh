use std::fmt;

/// An AMP address: `port.app.node.amp`
///
/// Examples:
/// - `clipboard.appmesh.cachyos.amp`
/// - `agent.markweb.mko.amp`
/// - `inbox.stalwart.mmc.amp`
///
/// The `.amp` suffix is the mesh TLD. Addresses are hierarchical:
/// - `node.amp` — the node itself
/// - `app.node.amp` — an application on a node
/// - `port.app.node.amp` — a specific port/endpoint within an app
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AmpAddress {
    pub port: Option<String>,
    pub app: Option<String>,
    pub node: String,
}

impl AmpAddress {
    /// Parse an AMP address string.
    ///
    /// Accepts:
    /// - `node.amp` → node only
    /// - `app.node.amp` → app + node
    /// - `port.app.node.amp` → port + app + node
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.strip_suffix(".amp")?;
        let parts: Vec<&str> = s.splitn(3, '.').collect();

        match parts.len() {
            1 => Some(Self {
                port: None,
                app: None,
                node: parts[0].to_string(),
            }),
            2 => Some(Self {
                port: None,
                app: Some(parts[0].to_string()),
                node: parts[1].to_string(),
            }),
            3 => Some(Self {
                port: Some(parts[0].to_string()),
                app: Some(parts[1].to_string()),
                node: parts[2].to_string(),
            }),
            _ => None,
        }
    }

    /// Check if this address targets a specific node.
    pub fn is_for_node(&self, node_name: &str) -> bool {
        self.node == node_name
    }

    /// Get the full address string.
    pub fn to_string_full(&self) -> String {
        format!("{self}")
    }
}

impl fmt::Display for AmpAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(port) = &self.port {
            write!(f, "{port}.")?;
        }
        if let Some(app) = &self.app {
            write!(f, "{app}.")?;
        }
        write!(f, "{}.amp", self.node)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_full_address() {
        let addr = AmpAddress::parse("clipboard.appmesh.cachyos.amp").unwrap();
        assert_eq!(addr.port.as_deref(), Some("clipboard"));
        assert_eq!(addr.app.as_deref(), Some("appmesh"));
        assert_eq!(addr.node, "cachyos");
        assert!(addr.is_for_node("cachyos"));
        assert_eq!(addr.to_string(), "clipboard.appmesh.cachyos.amp");
    }

    #[test]
    fn parse_app_address() {
        let addr = AmpAddress::parse("markweb.mko.amp").unwrap();
        assert_eq!(addr.port, None);
        assert_eq!(addr.app.as_deref(), Some("markweb"));
        assert_eq!(addr.node, "mko");
    }

    #[test]
    fn parse_node_address() {
        let addr = AmpAddress::parse("cachyos.amp").unwrap();
        assert_eq!(addr.port, None);
        assert_eq!(addr.app, None);
        assert_eq!(addr.node, "cachyos");
    }

    #[test]
    fn no_amp_suffix_fails() {
        assert!(AmpAddress::parse("cachyos.local").is_none());
        assert!(AmpAddress::parse("just-a-name").is_none());
    }
}
