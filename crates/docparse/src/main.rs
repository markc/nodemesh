mod extractors;

use clap::Parser;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "docparse", about = "Extract text from documents (PDF, DOCX, email, plaintext)")]
struct Cli {
    /// Path to the file to extract text from
    file: PathBuf,

    /// Output as JSON with metadata instead of plain text
    #[arg(long)]
    json: bool,
}

fn main() {
    let cli = Cli::parse();

    if !cli.file.exists() {
        eprintln!("Error: file not found: {}", cli.file.display());
        std::process::exit(1);
    }

    let extractor = match extractors::get_extractor(&cli.file) {
        Some(e) => e,
        None => {
            let ext = cli
                .file
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("(none)");
            eprintln!("Error: unsupported file type: .{ext}");
            std::process::exit(1);
        }
    };

    match extractor.extract(&cli.file) {
        Ok(doc) => {
            if cli.json {
                let json = serde_json::to_string_pretty(&doc).unwrap_or_else(|e| {
                    eprintln!("Error: JSON serialization failed: {e}");
                    std::process::exit(1);
                });
                println!("{json}");
            } else {
                print!("{}", doc.text);
            }
        }
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    }
}
