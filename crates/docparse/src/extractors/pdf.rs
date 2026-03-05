use std::path::Path;

use super::{DocumentMetadata, ExtractedDocument, Extractor};

pub struct PdfExtractor;

impl Extractor for PdfExtractor {
    fn extract(&self, path: &Path) -> Result<ExtractedDocument, String> {
        let bytes = std::fs::read(path).map_err(|e| format!("Failed to read file: {e}"))?;

        let text =
            pdf_extract::extract_text_from_mem(&bytes).map_err(|e| format!("PDF parse error: {e}"))?;

        let file_size = bytes.len() as u64;
        let filename = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown.pdf")
            .to_string();

        // Attempt to count pages by looking for /Type /Page entries
        let page_count = count_pdf_pages(&bytes);

        Ok(ExtractedDocument {
            text: normalize_whitespace(&text),
            metadata: DocumentMetadata {
                filename,
                mime_type: "application/pdf".to_string(),
                file_size,
                title: None,
                author: None,
                page_count,
                subject: None,
                date: None,
            },
        })
    }

}

fn normalize_whitespace(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut prev_newline_count = 0;

    for ch in text.chars() {
        if ch == '\n' {
            prev_newline_count += 1;
            if prev_newline_count <= 2 {
                result.push('\n');
            }
        } else {
            prev_newline_count = 0;
            result.push(ch);
        }
    }

    result.trim().to_string()
}

fn count_pdf_pages(bytes: &[u8]) -> Option<u32> {
    // Simple heuristic: count occurrences of "/Type /Page" (not "/Pages")
    let haystack = String::from_utf8_lossy(bytes);
    let count = haystack.matches("/Type /Page\n").count()
        + haystack.matches("/Type /Page\r").count()
        + haystack.matches("/Type /Page ").count()
        + haystack.matches("/Type/Page").count();

    if count > 0 {
        Some(count as u32)
    } else {
        None
    }
}
