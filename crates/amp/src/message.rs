use std::collections::HashMap;
use std::fmt;

/// An AMP wire message: frontmatter headers + optional markdown body.
///
/// Wire format:
/// ```text
/// ---
/// key: value
/// key: value
/// ---
/// optional body text
/// ```
///
/// The empty message `---\n---\n` (8 bytes) is valid — used as heartbeat/ACK/NOP.
#[derive(Debug, Clone, PartialEq)]
pub struct AmpMessage {
    pub headers: HashMap<String, String>,
    pub body: String,
}

/// The minimum valid AMP message — heartbeat, ACK, or keepalive.
pub const EMPTY_MESSAGE: &str = "---\n---\n";

impl AmpMessage {
    /// Create a new message with headers and body.
    pub fn new(headers: HashMap<String, String>, body: impl Into<String>) -> Self {
        Self {
            headers,
            body: body.into(),
        }
    }

    /// Create the empty AMP message (heartbeat/keepalive).
    pub fn empty() -> Self {
        Self {
            headers: HashMap::new(),
            body: String::new(),
        }
    }

    /// Create a command message (headers only, no body).
    pub fn command(headers: impl IntoIterator<Item = (&'static str, String)>) -> Self {
        Self {
            headers: headers.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
            body: String::new(),
        }
    }

    /// Parse a raw AMP wire message.
    ///
    /// Returns `None` if the input doesn't start with `---\n` or lacks a closing `---\n`.
    pub fn parse(raw: &str) -> Option<Self> {
        let content = raw.strip_prefix("---\n")?;

        let (frontmatter, body) = match content.split_once("\n---\n") {
            Some((fm, b)) => (fm, b),
            None => {
                // Could be `---\n---\n` (empty) — frontmatter is just `---`
                let content = content.strip_suffix("---\n")?;
                (content.trim_end_matches('\n'), "")
            }
        };

        let mut headers = HashMap::new();
        for line in frontmatter.lines() {
            if line.is_empty() {
                continue;
            }
            if let Some((k, v)) = line.split_once(": ") {
                headers.insert(k.trim().to_string(), v.trim().to_string());
            }
        }

        Some(Self {
            headers,
            body: body.to_string(),
        })
    }

    /// Serialize to AMP wire format.
    pub fn to_wire(&self) -> String {
        let mut out = String::from("---\n");
        for (k, v) in &self.headers {
            out.push_str(k);
            out.push_str(": ");
            out.push_str(v);
            out.push('\n');
        }
        out.push_str("---\n");
        if !self.body.is_empty() {
            out.push_str(&self.body);
            if !self.body.ends_with('\n') {
                out.push('\n');
            }
        }
        out
    }

    /// Check if this is the empty message (heartbeat/keepalive).
    pub fn is_empty_message(&self) -> bool {
        self.headers.is_empty() && self.body.is_empty()
    }

    /// Get a header value.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.headers.get(key).map(|s| s.as_str())
    }

    /// Get the `from` address.
    pub fn from_addr(&self) -> Option<&str> {
        self.get("from")
    }

    /// Get the `to` address.
    pub fn to_addr(&self) -> Option<&str> {
        self.get("to")
    }

    /// Get the `command`.
    pub fn command_name(&self) -> Option<&str> {
        self.get("command")
    }

    /// Get the `type` (request/response/event/stream).
    pub fn message_type(&self) -> Option<&str> {
        self.get("type")
    }

    /// Get `args` as parsed JSON.
    pub fn args(&self) -> Option<serde_json::Value> {
        self.get("args").and_then(|s| serde_json::from_str(s).ok())
    }

    /// Get `json` payload as parsed JSON.
    pub fn json_payload(&self) -> Option<serde_json::Value> {
        self.get("json").and_then(|s| serde_json::from_str(s).ok())
    }
}

impl fmt::Display for AmpMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_wire())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_empty_message() {
        let msg = AmpMessage::parse("---\n---\n").unwrap();
        assert!(msg.is_empty_message());
        assert_eq!(msg.to_wire(), "---\n---\n");
    }

    #[test]
    fn parse_command() {
        let raw = "---\namp: 1\ntype: request\nfrom: agent.markweb.cachyos.amp\nto: agent.markweb.mko.amp\ncommand: dispatch\n---\n";
        let msg = AmpMessage::parse(raw).unwrap();
        assert_eq!(msg.get("amp"), Some("1"));
        assert_eq!(msg.from_addr(), Some("agent.markweb.cachyos.amp"));
        assert_eq!(msg.to_addr(), Some("agent.markweb.mko.amp"));
        assert_eq!(msg.command_name(), Some("dispatch"));
        assert!(msg.body.is_empty());
    }

    #[test]
    fn parse_full_message() {
        let raw = "---\namp: 1\ntype: event\nfrom: screenshot.appmesh.cachyos.amp\ncommand: taken\n---\n# Screenshot saved\nPath: `/tmp/screenshot.png`\n";
        let msg = AmpMessage::parse(raw).unwrap();
        assert_eq!(msg.command_name(), Some("taken"));
        assert!(msg.body.starts_with("# Screenshot saved"));
    }

    #[test]
    fn parse_data_shape() {
        let raw = "---\njson: {\"count\": 12, \"unread\": 3}\n---\n";
        let msg = AmpMessage::parse(raw).unwrap();
        let json = msg.json_payload().unwrap();
        assert_eq!(json["count"], 12);
        assert_eq!(json["unread"], 3);
    }

    #[test]
    fn roundtrip() {
        let mut headers = HashMap::new();
        headers.insert("amp".to_string(), "1".to_string());
        headers.insert("type".to_string(), "request".to_string());
        headers.insert("command".to_string(), "ping".to_string());

        let msg = AmpMessage::new(headers, "hello world");
        let wire = msg.to_wire();
        let parsed = AmpMessage::parse(&wire).unwrap();

        assert_eq!(parsed.get("amp"), Some("1"));
        assert_eq!(parsed.command_name(), Some("ping"));
        assert_eq!(parsed.body.trim(), "hello world");
    }

    #[test]
    fn invalid_input_returns_none() {
        assert!(AmpMessage::parse("no frontmatter here").is_none());
        assert!(AmpMessage::parse("").is_none());
    }
}
