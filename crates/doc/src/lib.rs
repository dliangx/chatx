//! `doc` — Office document viewer core, built on **betteroffice** (Apache-2.0).
//!
//! Three output shapes, chosen by format:
//!
//! - **XLSX** — sheet-by-sheet cell grids ([`XlSheet`]s of [`XlRow`]s of
//!   [`XlCell`]s). betteroffice parses, recalculates and formats (values,
//!   fonts, fills, alignment, merges), and the app lays the grid out as a
//!   scrollable table. Rasterizing a long sheet to one image is both slow
//!   and memory-heavy, so it is not used here.
//! - **PPTX** — the betteroffice raster backend paints every slide to
//!   deterministic PNG bytes (`RenderedSlide`); the app displays the pages
//!   verbatim, so formatting is pixel-exact.
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

// The xlsx grid builder works on the raw model (betteroffice's `model()`
// output) and the target-agnostic render crate (display text + geometry),
// neither of which the xlsx facade re-exports wholesale. Both rename their
// lib target: the crates are `xlsx_model` / `xlsx_render`.
use xlsx_model::{CellRef as XlCellRef, CellValue as XlCellValue, DateSystem as XlDateSystem,
   Fill as XlFill, HAlign as XlHAlign, Sheet as XlSheetM, Stylesheet as XlStylesheet};

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

/// One rendered page (a PPTX slide): pixel-exact PNG bytes (`png`) with its
/// raster size (`width` × `height`).
#[derive(Clone, Debug)]
pub struct PngPage {
    pub width: u32,
    pub height: u32,
    pub png: Vec<u8>,
}

/// One cell of an XLSX sheet grid.
///
/// `w == 0` collapses the cell: a hidden column, or a position inside a
/// merged span whose anchor (top-left) carries the text.
#[derive(Clone, Debug)]
pub struct XlCell {
    /// Display text, number/date formatted (engine output; empty for blank cells).
    pub text: String,
    /// Column width in CSS px (0 = collapse).
    pub w: i32,
    /// Horizontal alignment: 0 left · 1 center · 2 right.
    pub align: i32,
    pub bold: bool,
    pub italic: bool,
    /// Font size in CSS px.
    pub size: i32,
    /// Cell fill as RGBA (`fill[3] == 0` → none).
    pub fill: [u8; 4],
    /// Text colour as RGBA (`color[3] == 0` → the viewer's default ink).
    pub color: [u8; 4],
}

impl XlCell {
    fn blank(width: i32) -> Self {
        Self {
            text: String::new(),
            w: width,
            align: 0,
            bold: false,
            italic: false,
            size: 15,
            fill: [0; 4],
            color: [0; 4],
        }
    }
}

/// One row of the grid: cells in column order plus a shared height.
#[derive(Clone, Debug)]
pub struct XlRow {
    /// Row height in CSS px.
    pub h: i32,
    pub cells: Vec<XlCell>,
}

/// One sheet of an XLSX workbook: name, the (possibly clipped) grid, total
/// grid width, and whether the preview budget trimmed the sheet.
#[derive(Clone, Debug)]
pub struct XlSheet {
    pub name: String,
    pub rows: Vec<XlRow>,
    pub truncated: bool,
    /// The grid's full width in CSS px (sum of the column widths) — the UI's
    /// scrollable content width.
    pub total_width: i32,
}

/// The parsed, Slint-ready document: a title line, a format tag, and either
/// DOCX [`Block`]s, PPTX [`PngPage`]s or XLSX [`XlSheet`]s (one of the three).
pub struct DocContent {
    /// The file's base name (shown as the document title).
    pub title: String,
    /// The document's extension upper-cased (e.g. `DOCX` / `XLSX` / `PPTX`), shown as a subtitle.
    pub format: String,
    /// DOCX content; empty otherwise.
    pub blocks: Vec<Block>,
    /// PPTX slides as rendered pages; empty otherwise.
    pub pages: Vec<PngPage>,
    /// XLSX sheets as cell grids; empty otherwise.
    pub sheets: Vec<XlSheet>,
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

/// Open the Office document at `path`. XLSX comes back as sheet [`XlSheet`]
/// grids, PPTX as rendered [`PngPage`]s, DOCX as a [`Block`] list. `Err`
/// carries a human-readable reason for the viewer status line (bad file,
/// unsupported format, …).
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

// ── XLSX: every readable sheet → a cell grid (the app draws it) ───────
//
// betteroffice does the heavy lifting (OOXML parse, formula recalculation,
// number/date formatting, style resolution); the result is plain data the
// app renders as a scrollable table. Rasterizing a long sheet to one image
// is both slow and memory-heavy, so it is deliberately not used here.

/// Preview budgets: grid cells are real UI items, so the biggest sheets are
/// clipped (and the `truncated` flag says so).
const XLSX_MAX_ROWS: u32 = 1000;
const XLSX_MAX_COLS: u32 = 80;
const XLSX_MAX_CELLS: u64 = 60_000;

fn open_xlsx(bytes: &[u8], title: String, format: String) -> Result<DocContent, String> {
    use betteroffice_xlsx::{CalculationOptions, Workbook};

    // Recalculate so formula cells carry their settled values before the
    // display-text formatting reads them.
    let workbook = Workbook::open_recalculated(bytes, CalculationOptions::default())
        .map_err(|e| e.to_string())?;
    let model = workbook.model();
    let mut sheets: Vec<XlSheet> = Vec::new();
    for sheet in &model.sheets {
        let xl = xl_sheet(sheet, &model.styles, model.date_system);
        if !xl.rows.is_empty() {
            sheets.push(xl);
        }
    }
    if sheets.is_empty() {
        return Err(
            "The workbook has no readable sheets (it may be empty or macro-only).".into(),
        );
    }
    Ok(DocContent { title, format, blocks: Vec::new(), pages: Vec::new(), sheets })
}

/// One sheet's grid: its used range (budget-clipped) as rows of dense cells
/// with display text, width and resolved style. Hidden rows / columns vanish,
/// merged spans collapse onto their anchor (top-left) cell.
fn xl_sheet(sheet: &XlSheetM, styles: &XlStylesheet, date_system: XlDateSystem) -> XlSheet {
    use xlsx_render::geometry as xr;

    let name = sheet.name.clone();
    let Some(used) = sheet.used_range() else {
        return XlSheet { name, rows: Vec::new(), truncated: false, total_width: 0 };
    };

    let first_row = used.start.row;
    let first_col = used.start.col;
    let mut truncated = false;
    let mut row_span = (used.end.row.saturating_sub(first_row) + 1).min(XLSX_MAX_ROWS);
    if used.end.row.saturating_sub(first_row) + 1 > XLSX_MAX_ROWS {
        truncated = true;
    }
    let col_span = (used.end.col.saturating_sub(first_col) + 1).min(XLSX_MAX_COLS);
    if used.end.col.saturating_sub(first_col) + 1 > XLSX_MAX_COLS {
        truncated = true;
    }
    // Cell budget: the grid is dense (rows × cols), so cap rows on top.
    let row_cap = (XLSX_MAX_CELLS / col_span.max(1) as u64) as u32;
    if row_span > row_cap {
        truncated = true;
        row_span = row_cap.max(1);
    }
    let last_row = first_row + row_span - 1;
    let last_col = first_col + col_span - 1;

    // Merged spans touching the visible window: anchor → span end (width
    // spans); every other covered position collapses onto the anchor.
    let mut anchors: std::collections::HashMap<(u32, u32), (u32, u32)> = std::collections::HashMap::new();
    let mut covered: std::collections::HashSet<(u32, u32)> = std::collections::HashSet::new();
    for m in &sheet.merges {
        if m.start.row == m.end.row && m.start.col == m.end.col {
            continue; // 1×1 "merge": no visual span.
        }
        if (first_row..=last_row).contains(&m.start.row)
            && (first_col..=last_col).contains(&m.start.col)
        {
            anchors.insert((m.start.row, m.start.col), (m.end.row, m.end.col));
        }
        let (r0, r1) = (m.start.row.max(first_row), m.end.row.min(last_row));
        let (c0, c1) = (m.start.col.max(first_col), m.end.col.min(last_col));
        if r0 <= r1 && c0 <= c1 {
            for r in r0..=r1 {
                for c in c0..=c1 {
                    if (r, c) != (m.start.row, m.start.col) {
                        covered.insert((r, c));
                    }
                }
            }
        }
    }

    // Column widths (character counts → px); hidden columns collapse to 0.
    let col_w: Vec<i32> = (first_col..=last_col)
        .map(|c| {
            if sheet.col_hidden(c) {
                0
            } else {
                let chars = sheet
                    .col_widths
                    .get(&c)
                    .copied()
                    .unwrap_or(xr::DEFAULT_COL_WIDTH_CHARS);
                xr::col_chars_to_px(chars).round() as i32
            }
        })
        .collect();
    let total_width = col_w.iter().sum::<i32>().max(1);

    let mut rows: Vec<XlRow> = Vec::new();
    for r in first_row..=last_row {
        if sheet.row_hidden(r) {
            continue;
        }
        let pt = sheet
            .row_heights
            .get(&r)
            .copied()
            .filter(|p| *p > 0.0)
            .unwrap_or(xr::DEFAULT_ROW_HEIGHT_PT);
        let h = xr::row_pt_to_px(pt).round() as i32;

        let mut cells: Vec<XlCell> = Vec::with_capacity(col_span as usize);
        let mut painted = false;
        for (i, c) in (first_col..=last_col).enumerate() {
            let pos = (r, c);
            if covered.contains(&pos) {
                cells.push(XlCell::blank(0)); // merged onto the anchor.
                continue;
            }
            // A merge anchor spans its (window-clipped) columns.
            let width = match anchors.get(&pos) {
                Some((_, end_c)) => {
                    let hi = (*end_c).min(last_col) as usize - first_col as usize;
                    col_w[..=hi].iter().sum()
                }
                None => col_w[i],
            };
            if width == 0 {
                cells.push(XlCell::blank(0)); // hidden column: data hidden too.
                continue;
            }
            let Some(cell) = sheet.cell(XlCellRef::new(r, c)) else {
                cells.push(XlCell::blank(width)); // blank slot; keeps alignment.
                continue;
            };
            let text = xlsx_render::display_text(styles, date_system, cell);
            // Spreadsheet defaults: numbers right, booleans center — applied
            // even when the cell carries no style, so `align` starts from
            // the value's kind and an explicit alignment overrides it.
            let mut out = XlCell {
                text,
                w: width,
                align: match cell.value {
                    XlCellValue::Number { .. } => 2,
                    XlCellValue::Bool { .. } => 1,
                    _ => 0,
                },
                bold: false,
                italic: false,
                size: 15,
                fill: [0; 4],
                color: [0; 4],
            };
            // Style: the cell's own xf, else its row / column default.
            if let Some(xf) = sheet
                .effective_style(XlCellRef::new(r, c))
                .and_then(|idx| styles.cell_xfs.get(idx as usize))
            {
                if let Some(font) = xf.font.and_then(|i| styles.fonts.get(i as usize)) {
                    out.bold = font.bold;
                    out.italic = font.italic;
                    let pt = font.size_pt.unwrap_or(11.0).clamp(6.0, 30.0);
                    out.size = (pt as f32 * (96.0_f32 / 72.0_f32)).round() as i32;
                    if let Some(hex) = font.color.as_ref().and_then(|c| c.resolve(&styles.theme)) {
                        out.color = hex_rgba(&hex, 255);
                    }
                }
                if let Some(fill) = xf.fill.and_then(|i| styles.fills.get(i as usize)) {
                    let color = match fill {
                        XlFill::Solid(c) => Some(c.clone()),
                        XlFill::Pattern { fg, .. } => fg.clone(),
                        XlFill::None => None,
                    };
                    if let Some(hex) = color.and_then(|c| c.resolve(&styles.theme)) {
                        out.fill = hex_rgba(&hex, 255);
                    }
                }
                if let Some(h) = xf.alignment.as_ref().and_then(|a| a.h) {
                    // `General` means “by value kind” — keep the default above.
                    out.align = match h {
                        XlHAlign::General => out.align,
                        XlHAlign::Right => 2,
                        XlHAlign::Center | XlHAlign::CenterContinuous | XlHAlign::Distributed => 1,
                        _ => 0,
                    };
                }
            }
            if !out.text.is_empty() || out.fill[3] != 0 {
                painted = true;
            }
            cells.push(out);
        }
        if painted {
            rows.push(XlRow { h, cells });
        }
    }

    XlSheet { name, rows, truncated, total_width }
}

/// `#rrggbb` → `[r, g, b, alpha]` (zeros when it doesn't parse).
fn hex_rgba(hex: &str, alpha: u8) -> [u8; 4] {
    let hex = hex.trim_start_matches('#');
    if hex.len() != 6 {
        return [0; 4];
    }
    let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).unwrap_or(0);
    [byte(0), byte(2), byte(4), alpha]
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
    Ok(DocContent { title, format, blocks: Vec::new(), pages, sheets: Vec::new() })
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
    Ok(DocContent { title, format, blocks, pages: Vec::new(), sheets: Vec::new() })
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

    #[test]
    fn hex_rgba_parses_rrggbb_and_refuses_garbage() {
        assert_eq!(hex_rgba("#eeeeee", 255), [0xee, 0xee, 0xee, 255]);
        assert_eq!(hex_rgba("#abc", 255), [0; 4]);
        assert_eq!(hex_rgba("#zz0011", 255), [0, 0, 0x11, 255]);
    }

    /// A styled, merged, hidden-column sheet comes back as a dense grid:
    /// the merge span collapses onto its anchor, the hidden column's data
    /// vanishes, number cells default to right alignment, fills resolve to
    /// RGBA.
    #[test]
    fn xl_sheet_renders_grid_merges_and_widths() {
        use xlsx_model::*;

        let mut sheet = Sheet::default();
        sheet.name = "Sheet1".into();
        sheet.col_widths = std::collections::BTreeMap::from([(0, 20.0), (1, 10.0), (3, 0.0)]);
        sheet.row_heights = std::collections::BTreeMap::from([(0, 30.0)]);
        sheet.merges = vec![CellRange::new(CellRef::new(0, 0), CellRef::new(0, 1))];

        let mut cells = std::collections::BTreeMap::new();
        cells.insert(
            (0, 0),
            Cell {
                value: CellValue::Text { value: "title".into() },
                formula: None,
                style: Some(1),
            },
        );
        cells.insert(
            (1, 2),
            Cell {
                value: CellValue::Number { value: 42.0 },
                formula: None,
                style: None,
            },
        );
        cells.insert(
            (1, 3),
            Cell {
                value: CellValue::Text { value: "hidden col".into() },
                formula: None,
                style: None,
            },
        );
        sheet.adopt_cells(cells);

        let mut styles = Stylesheet::default();
        styles.fonts = vec![
            Font::default(),
            Font {
                bold: true,
                size_pt: Some(14.0),
                color: Some(Color::Rgb("#ffffff".into())),
                ..Font::default()
            },
        ];
        styles.fills = vec![Fill::None, Fill::Solid(Color::Rgb("#eeeeee".into()))];
        styles.cell_xfs = vec![
            Xf::default(),
            Xf {
                font: Some(1),
                fill: Some(1),
                alignment: Some(Alignment { h: Some(HAlign::Left), ..Alignment::default() }),
                ..Xf::default()
            },
        ];

        let grid = xl_sheet(&sheet, &styles, DateSystem::V1900);
        assert_eq!(grid.name, "Sheet1");
        assert!(!grid.truncated);
        assert_eq!(grid.rows.len(), 2); // both rows carry a painted cell.

        // Row 0: the title anchor carries bold + fill + white ink over
        // columns 0–1; the covered slot collapses; the row is taller (30 pt).
        let row0 = &grid.rows[0];
        assert_eq!(row0.h, 40); // 30 pt × 96/72
        let title = &row0.cells[0];
        assert_eq!(title.text, "title");
        assert!(title.bold);
        assert_eq!(title.size, 19); // 14 pt
        assert_eq!(title.fill, [0xee, 0xee, 0xee, 255]);
        assert_eq!(title.color, [0xff, 0xff, 0xff, 255]);
        assert_eq!(title.align, 0);
        // The anchor spans cols 0+1: everything but cols 2–3.
        assert_eq!(title.w, grid.total_width - row0.cells[2].w - row0.cells[3].w);
        assert_eq!(row0.cells[1].w, 0); // merged onto the anchor.

        // Row 1: the number is right-aligned by default (no explicit style);
        // the hidden column's (width 0) data is dropped with it.
        let amount = &grid.rows[1].cells[2];
        assert_eq!(amount.text, "42");
        assert_eq!(amount.align, 2);
        assert!(!grid.rows.iter().any(|r| r.cells.iter().any(|c| c.text.contains("hidden"))));
        assert_eq!(grid.rows[1].cells[3].w, 0);
    }
}
