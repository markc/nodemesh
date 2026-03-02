use crate::AmpMessage;

/// Known AMP header fields.
pub const KNOWN_HEADERS: &[&str] = &[
    "amp", "type", "id", "from", "to", "command", "args", "json", "reply-to", "ttl", "error",
    "timestamp",
];

/// Valid message types.
pub const VALID_TYPES: &[&str] = &["request", "response", "event", "stream"];

/// Validate an AMP message for protocol conformance.
///
/// Returns a list of warnings (not errors — AMP is permissive).
/// An empty Vec means the message is fully conformant.
pub fn validate(msg: &AmpMessage) -> Vec<String> {
    let mut warnings = Vec::new();

    // Empty messages are always valid
    if msg.is_empty_message() {
        return warnings;
    }

    // Check for unknown headers
    for key in msg.headers.keys() {
        if !KNOWN_HEADERS.contains(&key.as_str()) {
            warnings.push(format!("unknown header: {key}"));
        }
    }

    // Validate type field
    if let Some(msg_type) = msg.get("type") {
        if !VALID_TYPES.contains(&msg_type) {
            warnings.push(format!("invalid type: {msg_type}"));
        }
    }

    // Validate args is valid JSON
    if let Some(args) = msg.get("args") {
        if serde_json::from_str::<serde_json::Value>(args).is_err() {
            warnings.push("args is not valid JSON".to_string());
        }
    }

    // Validate json payload is valid JSON
    if let Some(json) = msg.get("json") {
        if serde_json::from_str::<serde_json::Value>(json).is_err() {
            warnings.push("json payload is not valid JSON".to_string());
        }
    }

    // Validate ttl is numeric
    if let Some(ttl) = msg.get("ttl") {
        if ttl.parse::<u32>().is_err() {
            warnings.push(format!("ttl is not a valid integer: {ttl}"));
        }
    }

    warnings
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_message_no_warnings() {
        let msg = AmpMessage::parse(
            "---\namp: 1\ntype: request\ncommand: ping\n---\n",
        )
        .unwrap();
        assert!(validate(&msg).is_empty());
    }

    #[test]
    fn unknown_header_warns() {
        let msg = AmpMessage::parse("---\nfoo: bar\n---\n").unwrap();
        let warnings = validate(&msg);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("unknown header: foo"));
    }

    #[test]
    fn invalid_type_warns() {
        let msg = AmpMessage::parse("---\ntype: banana\n---\n").unwrap();
        let warnings = validate(&msg);
        assert!(warnings.iter().any(|w| w.contains("invalid type")));
    }

    #[test]
    fn empty_message_always_valid() {
        let msg = AmpMessage::empty();
        assert!(validate(&msg).is_empty());
    }
}
