use std::path::Path;

use super::{DocumentMetadata, ExtractedDocument, Extractor};

pub struct EmailExtractor;

impl Extractor for EmailExtractor {
    fn extract(&self, path: &Path) -> Result<ExtractedDocument, String> {
        let bytes = std::fs::read(path).map_err(|e| format!("Failed to read file: {e}"))?;
        let file_size = bytes.len() as u64;
        let filename = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown.eml")
            .to_string();

        let message = mail_parser::MessageParser::default()
            .parse(&bytes)
            .ok_or_else(|| "Failed to parse email".to_string())?;

        let subject = message.subject().map(|s| s.to_string());
        let from = message
            .from()
            .and_then(|addrs| addrs.first())
            .and_then(|addr| addr.address())
            .map(|a| a.to_string());
        let date = message.date().map(|d| d.to_rfc3339());

        // Build text from headers + body
        let mut parts: Vec<String> = Vec::new();

        if let Some(ref subj) = subject {
            parts.push(format!("Subject: {subj}"));
        }
        if let Some(ref f) = from {
            parts.push(format!("From: {f}"));
        }
        if let Some(ref d) = date {
            parts.push(format!("Date: {d}"));
        }

        if !parts.is_empty() {
            parts.push(String::new()); // blank line separator
        }

        // Prefer text/plain body, fall back to text/html stripped of tags
        if let Some(body) = message.body_text(0) {
            parts.push(body.to_string());
        } else if let Some(body) = message.body_html(0) {
            parts.push(strip_html_tags(&body));
        }

        let text = parts.join("\n");

        Ok(ExtractedDocument {
            text,
            metadata: DocumentMetadata {
                filename,
                mime_type: "message/rfc822".to_string(),
                file_size,
                title: subject,
                author: from,
                page_count: None,
                subject: None,
                date,
            },
        })
    }

}

fn strip_html_tags(html: &str) -> String {
    let mut result = String::with_capacity(html.len());
    let mut in_tag = false;

    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => result.push(ch),
            _ => {}
        }
    }

    result
}
