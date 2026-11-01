//! `doc` — Office document viewer core, built on **betteroffice** (Apache-2.0).
//!
//! Two output shapes, chosen by format:
//!
//! - **XLSX / PPTX** — the betteroffice raster backend paints every sheet /
//!   slide to deterministic PNG bytes (`RenderedPng`/`RenderedSlide`). The
//!   app displays the pages verbatim, so formatting is pixel-exact.
//! - **DOCX** — the native supported read path is the high-fidelity
//!   *structured export* (betteroffice-docx `Document::export_structured`):
//!   headings with outline level, lists with markers, tables with cells,
//!   inline runs. `open_docx` flattens that into a small list of [`Block`]s
//!   the app renders as a formatted document (betteroffice-docx's own PNG
//!   pipeline is tied to the collaborative editing engine — its reference
//!   viewer is a 120 KB custom app — which is out of scope for a chat
//!   preview).
//!
//! Legacy Office (`.doc` / `.xls` / `.ppt`) is OOXML-only territory and is
//! refused with a clear message. No slint, no retained state — the caller
//! owns whatever `open_doc` returns.
//!
//! Carlito (SIL OFL, in `assets/`) is the Calibri metric-compatible face the
//! published betteroffice bundles use in their own raster tests; the PPTX
//! backend needs a caller-registered face, and the XLSX backend carries its
//! own copy, so this one face covers the PPTX gap.

use std::path::Path;

const CARLITO: &[u8] = include_bytes!("../assets/Carlito-Regular.ttf");

// ── Public model ──────────────────────────────────────────────────────

/// Visual kind of a rendered block — the `DocBlock.kind` integers on the Slint side.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    #[default]
    Heading = 0,
    Paragraph = 1,
    Bullet = 2,
    Numbered = 3,
    Rule = 5,
    Table = 8,
}

impl Kind {
    /// The integer a UI binds the kind to (the `Kind` repr value).
    pub fn number(self) -> i32 {
        match self {
            Kind::Heading => 0,
            Kind::Paragraph => 1,
            Kind::Bullet => 2,
            Kind::Numbered => 3,
            Kind::Rule => 5,
            Kind::Table => 8,
        }
    }
}

/// One visual block of a DOCX document — a heading, paragraph, list item, a
/// page / section break (rule), or a table (`cells` is the flat table body,
/// row-major; `header_rows` = how many leading rows render bold + tinted).
/// Text is **plain** — Slint has no rich-text element, so block-level
/// formatting (size / weight / indent) carries the emphasis.
#[derive(Clone, Debug)]
pub struct Block {
    pub kind: Kind,
    /// Heading level (1–6) for headings; list nesting depth (0 = top) for list items.
    pub level: i32,
    /// Display text; empty for tables and rules.
    pub text: String,
    /// Table body: one inner array per row, one string per cell. Empty otherwise.
    pub cells: Vec<Vec<String>>,
    /// For `Kind::Table`: leading rows rendered bold + tinted.
    pub header_rows: i32,
}

impl Block {
    fn text(kind: Kind, level: i32, text: String) -> Self {
        Self {
            kind,
            level,
            text,
            cells: Vec::new(),
            header_rows: 0,
        }
    }

    fn table(cells: Vec<Vec<String>>, header_rows: i32) -> Self {
        Self {
            kind: Kind::Table,
            level: 0,
            text: String::new(),
            cells,
            header_rows,
        }
    }
}

/// One rendered page (an XLSX sheet or a PPTX slide): pixel-exact PNG bytes
/// (`png`) with its raster size (`width` × `height`).
#[derive(Clone, Debug)]
pub struct PngPage {
    pub width: u32,
    pub height: u32,
    pub png: Vec<u8>,
}

/// The parsed, Slint-ready document: a title line, a format tag, and either
/// DOCX [`Block`]s or XLSX/PPTX [`PngPage`]s (never both).
pub struct DocContent {
    /// The file's base name (shown as the document title).
    pub title: String,
    /// The document's extension upper-cased (e.g. `DOCX` / `XLSX` / `PPTX`), shown as a subtitle.
    pub format: String,
    /// DOCX content; empty for XLSX / PPTX.
    pub blocks: Vec<Block>,
    /// XLSX / PPTX rendered pages; empty for DOCX.
    pub pages: Vec<PngPage>,
}

/// Whether the path has one of the OOXML extensions betteroffice can open.
pub fn is_office_ext(path: &str) -> bool {
    matches!(doc_ext_of(path).as_str(), "docx" | "xlsx" | "pptx")
}

/// Lower-cased extension of a path (empty string when there is none).
pub fn doc_ext_of(path: &str) -> String {
    Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase()
}

/// Open the Office document at `path`. XLSX / PPTX come back as rendered
/// [`PngPage`]s; DOCX as a [`Block`] list. `Err` carries a human-readable
/// reason for the viewer status line (bad file, unsupported format, …).
pub fn open_doc(path: &str) -> Result<DocContent, String> {
    let title = Path::new(path)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "document".to_string());
    let format = doc_ext_of(path).to_ascii_uppercase();
    let bytes = std::fs::read(path).map_err(|e| format!("cannot read the file: {e}"))?;
    match doc_ext_of(path).as_str() {
        "xlsx" => open_xlsx(&bytes, title, format),
        "pptx" => open_pptx(&bytes, title, format),
        "docx" => open_docx(&bytes, title, format),
        other => Err(format!(
            ".{other} isn't supported — convert it to .docx/.xlsx/.pptx to view it here."
        )),
    }
}

// ── XLSX: every visible sheet → one PNG page ──────────────────────────

fn open_xlsx(bytes: &[u8], title: String, format: String) -> Result<DocContent, String> {
    use betteroffice_xlsx::{CalculationOptions, RenderOptions, SheetId, Workbook};

    let workbook = Workbook::open_recalculated(bytes, CalculationOptions::default())
        .map_err(|e| e.to_string())?;
    let mut pages: Vec<PngPage> = Vec::new();
    for i in 0..workbook.sheet_count() {
        let id = SheetId(i as u32);
        if let Some(png) = render_fitted(&|scale: f32| {
            workbook
                .render_sheet(id, &RenderOptions { scale, ..RenderOptions::default() })
                .map(|p| p)
                .map_err(|e| e.to_string())
        }) {
            pages.push(PngPage {
                width: png.width,
                height: png.height,
                png: png.bytes,
            });
        }
    }
    if pages.is_empty() {
        return Err("The workbook has no renderable sheets (it may be empty or macro-only).".into());
    }
    Ok(DocContent { title, format, blocks: Vec::new(), pages })
}

/// The betteroffice raster backend refuses a surface that would exceed its
/// pixel budget (16384 px per side, 16 Mpx); try a scale ladder from hidpi
/// down so the biggest safe render wins.
fn render_fitted<R, E>(render: &impl Fn(f32) -> Result<R, E>) -> Option<R>
where
    E: core::fmt::Display,
{
    for scale in [2.0_f32, 1.75, 1.5, 1.0, 0.75] {
        match render(scale) {
            Ok(page) => return Some(page),
            // Budget refusals, and empty/hidden sheets (no used range), both
            // land here and are retried at the next scale.
            Err(_) => {}
        }
    }
    None
}

// ── PPTX: every slide → one PNG page (16:9 decks fit at 2× = 2560×1440) ──

fn open_pptx(bytes: &[u8], title: String, format: String) -> Result<DocContent, String> {
    use betteroffice_pptx::{Presentation, RenderOptions};

    let mut deck = Presentation::open(bytes).map_err(|e| e.to_string())?;
    // At least one registered face is required to lay out text; register
    // every style chain so bold / italic runs resolve too (one face per
    // family — `Presentation::register_font` replaces, unlike DOCX's chains).
    for (bold, italic) in [(false, false), (true, false), (false, true), (true, true)] {
        deck.register_font("Carlito", bold, italic, CARLITO)
            .map_err(|e| e.to_string())?;
    }
    let slide_count = deck.slides().len();
    if slide_count == 0 {
        return Err("The presentation has no slides.".into());
    }
    let mut pages: Vec<PngPage> = Vec::new();
    for index in 0..slide_count {
        let Some(png) = render_fitted(&|scale: f32| {
            deck.render_png(index, &RenderOptions { scale, ..RenderOptions::default() })
                .map_err(|e| e.to_string())
        })
        else {
            return Err(format!("slide {} is too large to render.", index + 1));
        };
        pages.push(PngPage {
            width: png.width,
            height: png.height,
            png: png.bytes,
        });
    }
    Ok(DocContent { title, format, blocks: Vec::new(), pages })
}

// ── DOCX: betteroffice structured export → blocks ─────────────────────

fn open_docx(bytes: &[u8], title: String, format: String) -> Result<DocContent, String> {
    use betteroffice_docx::{Document, ExportOptions, RevisionView};

    let doc = Document::open(bytes).map_err(|e| e.to_string())?;
    let content = doc
        .export_structured(&ExportOptions::new(RevisionView::Accepted))
        .map_err(|e| e.to_string())?;

    let mut blocks: Vec<Block> = Vec::new();
    for story in &content.stories {
        // The body is the document; headers / footers / notes are page
        // furniture and annotations — skipping them keeps the preview honest.
        if story.kind != StoryKind::Body {
            continue;
        }
        for block in &story.blocks {
            push_block(&block.content, &mut blocks);
        }
    }
    if blocks.is_empty() {
        return Err("The document has no readable content.".into());
    }
    Ok(DocContent { title, format, blocks, pages: Vec::new() })
}

/// Walk one structured [`BoBlockKind`] into flat viewer blocks. Nested
/// content controls recurse; tables are flattened cell-by-cell; unsupported
/// constructs are dropped rather than failing the whole document.
fn push_block(content: &BoBlockKind, out: &mut Vec<Block>) {
    match content {
        BoBlockKind::Heading { paragraph, heading } => {
            let text = inline_text(&paragraph.inlines);
            if text.trim().is_empty() {
                return;
            }
            let level = ((heading.outline_level as i32) + 1).clamp(1, 6);
            out.push(Block::text(Kind::Heading, level, text));
        }
        BoBlockKind::Paragraph { paragraph } => {
            let text = inline_text(&paragraph.inlines);
            if !text.trim().is_empty() {
                out.push(Block::text(Kind::Paragraph, 0, text));
            }
        }
        BoBlockKind::ListItem { paragraph, list, .. } => {
            let text = inline_text(&paragraph.inlines);
            if text.trim().is_empty() {
                return;
            }
            let marker = list
                .marker
                .clone()
                .unwrap_or_default()
                .trim()
                .to_string();
            let ordered = list.format != "bullet";
            let marker = if marker.is_empty() {
                if ordered {
                    "1.".to_string()
                } else {
                    "•".to_string()
                }
            } else {
                marker
            };
            let kind = if ordered { Kind::Numbered } else { Kind::Bullet };
            out.push(Block::text(kind, list.level.min(8) as i32, format!("{marker} {text}")));
        }
        BoBlockKind::Table { table } => {
            let mut cells: Vec<Vec<String>> = Vec::new();
            for row in &table.rows {
                let width = (table.grid_columns).max(1) as usize;
                let mut line: Vec<String> = vec![String::new(); width];
                for cell in &row.cells {
                    let col = (cell.column as usize).min(width - 1);
                    line[col] = cell_text(&cell.blocks);
                }
                cells.push(line);
            }
            if !cells.is_empty() {
                let header_rows = if table.rows.first().is_some_and(|r| r.header) {
                    1
                } else {
                    0
                };
                out.push(Block::table(cells, header_rows));
            }
        }
        BoBlockKind::ContentControl { blocks, .. } => {
            for block in blocks {
                push_block(&block.content, out);
            }
        }
        // Page breaks read as visual rules; section / column furniture does not.
        BoBlockKind::Break { break_type } => {
            if matches!(break_type, BoBreakType::Page) {
                out.push(Block::text(Kind::Rule, 0, String::new()));
            }
        }
        BoBlockKind::SectionBreak { .. } | BoBlockKind::Unsupported { .. } => {}
    }
}

/// A table cell may hold several paragraphs, or even a nested table; flatten
/// its blocks to line-separated text (nested tables lose their grid — the
/// viewer's cells are flat strings).
fn cell_text(blocks: &[BoBlock]) -> String {
    use BoBlockKind as BK;

    let mut parts: Vec<String> = Vec::new();
    for block in blocks {
        match &block.content {
            BK::Paragraph { paragraph } | BK::Heading { paragraph, .. } => {
                let t = inline_text(&paragraph.inlines);
                if !t.trim().is_empty() {
                    parts.push(t);
                }
            }
            BK::ListItem { paragraph, .. } => {
                let t = inline_text(&paragraph.inlines);
                if !t.trim().is_empty() {
                    parts.push(t);
                }
            }
            BK::Table { table } => {
                for row in &table.rows {
                    let line: Vec<String> = row
                        .cells
                        .iter()
                        .map(|c| cell_text(&c.blocks))
                        .collect();
                    if line.iter().any(|c| !c.trim().is_empty()) {
                        parts.push(line.join("  "));
                    }
                }
            }
            BK::ContentControl { blocks, .. } => {
                let t = cell_text(blocks);
                if !t.trim().is_empty() {
                    parts.push(t);
                }
            }
            BK::Break { .. } | BK::SectionBreak { .. } | BK::Unsupported { .. } => {}
        }
    }
    parts.join("\n")
}

/// One inline's plain text: text runs verbatim, tabs as spaces, line breaks
/// preserved, images as a bracketed caption; notes / comments / fields are
/// annotations, not body copy.
fn inline_text(inlines: &[BoInline]) -> String {
    use BoInlineKind as IK;

    let mut out = String::new();
    for inline in inlines {
        match &inline.content {
            IK::Text { text } => out.push_str(text),
            IK::Tab => out.push(' '),
            IK::Break { break_type } => {
                if matches!(break_type, BoBreakType::Line) {
                    out.push('\n');
                }
            }
            IK::Image { alt_text, .. } => {
                let alt = alt_text.as_deref().map(|a| a.trim()).unwrap_or("");
                if alt.is_empty() {
                    out.push_str("[image] ");
                } else {
                    out.push_str("[image: ");
                    out.push_str(alt);
                    out.push_str("] ");
                }
            }
            IK::ContentControl { inlines, .. } => out.push_str(&inline_text(inlines)),
            IK::NoteReference { .. } | IK::CommentReference { .. } | IK::Field { .. }
            | IK::Unsupported { .. } => {}
        }
    }
    out
}

// betteroffice-docx-edit's structured types (the read model the DOCX facade
// hands back) — aliased so they don't collide with this crate's own `Block`.
pub(crate) use betteroffice_docx::structured::{
    Block as BoBlock,
    BlockKind as BoBlockKind,
    BreakType as BoBreakType,
    Inline as BoInline,
    InlineKind as BoInlineKind,
    StoryKind,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn office_exts_are_the_three_ooxml_formats_only() {
        assert!(is_office_ext("/tmp/a.docx"));
        assert!(is_office_ext("report.XLSX"));
        assert!(is_office_ext("deck.pptx"));
        // Legacy binary Office is betteroffice's non-territory.
        assert!(!is_office_ext("old.doc"));
        assert!(!is_office_ext("old.xls"));
        assert!(!is_office_ext("old.ppt"));
        assert!(!is_office_ext("other.pdf"));
        assert!(!is_office_ext("noext"));
    }

    #[test]
    fn kind_numbers_match_the_slint_side() {
        assert_eq!(Kind::Heading.number(), 0);
        assert_eq!(Kind::Paragraph.number(), 1);
        assert_eq!(Kind::Bullet.number(), 2);
        assert_eq!(Kind::Numbered.number(), 3);
        assert_eq!(Kind::Rule.number(), 5);
        assert_eq!(Kind::Table.number(), 8);
    }

    #[test]
    fn table_blocks_carry_their_grid() {
        let b = Block::table(
            vec![vec!["a".into(), "b".into()], vec!["1".into(), "2".into()]],
            1,
        );
        assert_eq!(b.cells.len(), 2);
        assert_eq!(b.cells[0].len(), 2);
        assert_eq!(b.header_rows, 1);
        assert_eq!(b.text, "");
    }
}
