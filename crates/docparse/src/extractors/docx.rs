use std::path::Path;

use super::{DocumentMetadata, ExtractedDocument, Extractor};

pub struct DocxExtractor;

impl Extractor for DocxExtractor {
    fn extract(&self, path: &Path) -> Result<ExtractedDocument, String> {
        let bytes = std::fs::read(path).map_err(|e| format!("Failed to read file: {e}"))?;
        let file_size = bytes.len() as u64;
        let filename = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown.docx")
            .to_string();

        let docx = docx_rs::read_docx(&bytes).map_err(|e| format!("DOCX parse error: {e}"))?;

        let mut paragraphs: Vec<String> = Vec::new();

        for child in docx.document.children.iter() {
            if let docx_rs::DocumentChild::Paragraph(para) = child {
                let mut para_text = String::new();
                for pc in para.children.iter() {
                    if let docx_rs::ParagraphChild::Run(run) = pc {
                        for rc in run.children.iter() {
                            if let docx_rs::RunChild::Text(text) = rc {
                                para_text.push_str(&text.text);
                            }
                        }
                    }
                }
                if !para_text.is_empty() {
                    paragraphs.push(para_text);
                }
            }
        }

        let text = paragraphs.join("\n\n");

        Ok(ExtractedDocument {
            text,
            metadata: DocumentMetadata {
                filename,
                mime_type:
                    "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
                        .to_string(),
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
