pub mod docx;
pub mod email;
pub mod pdf;
pub mod plaintext;

use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct ExtractedDocument {
    pub text: String,
    pub metadata: DocumentMetadata,
}

#[derive(Debug, Serialize)]
pub struct DocumentMetadata {
    pub filename: String,
    pub mime_type: String,
    pub file_size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page_count: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub date: Option<String>,
}

pub trait Extractor {
    fn extract(&self, path: &std::path::Path) -> Result<ExtractedDocument, String>;
}

pub fn get_extractor(path: &std::path::Path) -> Option<Box<dyn Extractor>> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .unwrap_or_default();

    match ext.as_str() {
        "pdf" => Some(Box::new(pdf::PdfExtractor)),
        "docx" => Some(Box::new(docx::DocxExtractor)),
        "eml" | "msg" => Some(Box::new(email::EmailExtractor)),
        "txt" | "md" | "csv" | "log" | "json" | "xml" | "html" | "htm" | "yaml" | "yml"
        | "toml" | "ini" | "cfg" | "conf" | "rst" | "tex" => {
            Some(Box::new(plaintext::PlaintextExtractor))
        }
        _ => None,
    }
}
