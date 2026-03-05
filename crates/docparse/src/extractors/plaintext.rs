use std::path::Path;

use super::{DocumentMetadata, ExtractedDocument, Extractor};

pub struct PlaintextExtractor;

impl Extractor for PlaintextExtractor {
    fn extract(&self, path: &Path) -> Result<ExtractedDocument, String> {
        let text =
            std::fs::read_to_string(path).map_err(|e| format!("Failed to read file: {e}"))?;
        let file_size = text.len() as u64;
        let filename = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown.txt")
            .to_string();

        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("txt");

        let mime_type = match ext {
            "md" => "text/markdown",
            "csv" => "text/csv",
            "json" => "application/json",
            "xml" => "application/xml",
            "html" | "htm" => "text/html",
            "yaml" | "yml" => "text/yaml",
            "toml" => "text/toml",
            _ => "text/plain",
        }
        .to_string();

        Ok(ExtractedDocument {
            text,
            metadata: DocumentMetadata {
                filename,
                mime_type,
                file_size,
                title: None,
                author: None,
                page_count: None,
                subject: None,
                date: None,
            },
        })
    }

}
