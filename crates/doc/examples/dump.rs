//! Render / extract an Office document through the `doc` crate:
//!
//! - XLSX / PPTX → PNG page files next to the input (`<stem>.pages-NNN.png`).
//! - DOCX → the extracted blocks, printed one per line.
//!
//! Usage: `cargo run -q -p doc --example dump -- <file> [out-prefix]`

use std::path::{Path, PathBuf};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(path) = args.first() else {
        eprintln!("usage: dump <file>");
        std::process::exit(2);
    };
    match doc::open_doc(path) {
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
        Ok(content) => {
            if !content.pages.is_empty() {
                let stem = Path::new(path)
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "doc".to_string());
                for (i, page) in content.pages.iter().enumerate() {
                    let out: PathBuf = Path::new(path)
                        .parent()
                        .unwrap_or(Path::new("."))
                        .join(format!("{stem}.pages-{i:03}.png"));
                    match std::fs::write(&out, &page.png) {
                        Ok(()) => println!(
                            "page {i}: {}x{} -> {}",
                            page.width,
                            page.height,
                            out.display()
                        ),
                        Err(e) => eprintln!("page {i}: write failed: {e}"),
                    }
                }
            } else {
                println!("{}  ({} blocks)", content.format, content.blocks.len());
                for (i, block) in content.blocks.iter().enumerate() {
                    let text = if block.cells.is_empty() {
                        block.text.replace('\n', "⏎ ")
                    } else {
                        format!(
                            "<table {}x{} header={}>",
                            block.cells.len(),
                            block.cells.first().map(|r| r.len()).unwrap_or(0),
                            block.header_rows
                        )
                    };
                    println!("{i:3} [{} lv{}] {text}", block.kind.number(), block.level);
                }
            }
        }
    }
}
