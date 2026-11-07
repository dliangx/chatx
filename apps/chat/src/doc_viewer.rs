//! Office document viewer wiring (docx / xlsx / pptx).
//!
//! Public API used by `lib.rs`:
//!
//! - [`is_office_ext`]: platform-agnostic suffix sniff.
//! - [`doc_pages_clear`] / [`xl_sheets_clear`]: no-op on mobile, clears the
//!   thread-local caches on desktop (called from the `on_close_doc`
//!   callback).
//!
//! Everything else is `pub(super)` and consumed from inside the
//! callbacks in `lib.rs` (`on_copy_text`, `on_page_at`, `open_doc_page`).
//!
//! Mobile gate: the `doc` crate (betteroffice) is **not** a dependency of
//! `chatx` on Android / iOS. All the `doc::`-typed helpers below are
//! `#[cfg(not(any(target_os = "android", target_os = "ios")))]`. Mobile
//! callers reach office files through the OS reveal path instead, so none of
//! these are ever invoked off the desktop target.

#[cfg(not(any(target_os = "android", target_os = "ios")))]
use slint::{ComponentHandle, SharedString};
#[cfg(not(any(target_os = "android", target_os = "ios")))]
use std::cell::RefCell;

// ─── shared (all platforms) ─────────────────────────────────────────────────

/// Tiny office-extension sniff used by both platforms (the `doc` crate's
/// `is_office_ext` is not reachable on mobile, and duplicating the check
/// here avoids dragging in the whole betteroffice stack just to match a
/// suffix).
pub fn is_office_ext(path: &str) -> bool {
    match std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some(x) => matches!(x, "docx" | "xlsx" | "pptx" | "doc" | "xls" | "ppt"),
        None => false,
    }
}

// ─── desktop-only ───────────────────────────────────────────────────────────

// Rendered Office-document pages (PPTX slides) of the open page-mode viewer.
// `SlintImage` is `!Send`, so the page bytes live in a thread-local that only
// the event-loop thread ever touches: the page callbacks and `open_doc_page`
// both run there.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
thread_local! {
    static DOC_PAGES: RefCell<Option<Vec<doc::PngPage>>> =
        const { RefCell::new(None) };
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub fn doc_pages_replace(pages: Option<Vec<doc::PngPage>>) {
    DOC_PAGES.with(|c| *c.borrow_mut() = pages);
}

/// Unconditional wrapper so callers (e.g. `on_close_doc`) compile on every
/// target. On mobile the body is empty (the cache itself is not linked in).
#[allow(dead_code)]
pub fn doc_pages_clear() {
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        doc_pages_replace(None);
    }
}

/// One page by index (`None` when the cache is empty or the index out of range).
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub fn doc_page_get(index: i32) -> Option<doc::PngPage> {
    if index < 0 {
        return None;
    }
    DOC_PAGES.with(|c| {
        c.borrow()
            .as_ref()
            .and_then(|v| v.get(index as usize).cloned())
    })
}

// XLSX sheet grids of the open sheet-mode viewer; same thread-local
// discipline as the page cache above.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
thread_local! {
    static XL_SHEETS: RefCell<Option<Vec<doc::XlSheet>>> =
        const { RefCell::new(None) };
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub fn xl_sheets_replace(sheets: Option<Vec<doc::XlSheet>>) {
    XL_SHEETS.with(|c| *c.borrow_mut() = sheets);
}

/// Unconditional wrapper so callers compile on every target.
#[allow(dead_code)]
pub fn xl_sheets_clear() {
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        xl_sheets_replace(None);
    }
}

/// One sheet by index (`None` when the cache is empty or the index out of range).
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub fn xl_sheet_get(index: i32) -> Option<doc::XlSheet> {
    if index < 0 {
        return None;
    }
    XL_SHEETS.with(|c| {
        c.borrow()
            .as_ref()
            .and_then(|v| v.get(index as usize).cloned())
    })
}

/// Fill / tint RGBA quadruple → the slint 3-value color model (`[r, g, b]`;
/// empty vector = "none", which the UI renders as transparent / default ink).
#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn xl_rgba(v: [u8; 4]) -> slint::ModelRc<i32> {
    let v = if v[3] == 0 {
        Vec::<i32>::new()
    } else {
        vec![v[0] as i32, v[1] as i32, v[2] as i32]
    };
    slint::ModelRc::new(slint::VecModel::from(v))
}

/// Sheet grid → nested shared models: a model of rows, each row a model of
/// its cells (the Slint array-in-struct convention, cf. `DocBlock.cells`).
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub fn xl_slint_rows(sheet: &doc::XlSheet) -> slint::ModelRc<crate::XlRow> {
    let rows: Vec<crate::XlRow> = sheet
        .rows
        .iter()
        .map(|r| {
            let cell_items: Vec<crate::XlCell> = r
                .cells
                .iter()
                .map(|c| crate::XlCell {
                    text: SharedString::from(c.text.clone()),
                    w: c.w,
                    align: c.align,
                    bold: c.bold,
                    italic: c.italic,
                    size: c.size,
                    fill: xl_rgba(c.fill),
                    tint: xl_rgba(c.color),
                })
                .collect();
            crate::XlRow {
                h: r.h,
                cells: slint::ModelRc::new(slint::VecModel::from(cell_items)),
            }
        })
        .collect();
    slint::ModelRc::new(slint::VecModel::from(rows))
}

/// Open the Office-document viewer (`doc` crate: betteroffice) on `path` —
/// XLSX to in-app cell grids (sheet view), PPTX to pixel-exact PNG pages
/// (page view), DOCX to a formatted block view (block view). A re-open after
/// a minimise just re-shows the (still-loaded) overlay.
///
/// Desktop-only entry point (the outermost gate): mobile builds route Office
/// files to the OS reveal path instead (see `on_open_doc` and the chat-file
/// handler in `lib.rs`), so this function isn't reachable there.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub fn open_doc_page(ui: &crate::MainWindow, path: &str) {
    let dh = ui.global::<crate::DocHost>();
    // Showing a (re-)opened overlay dismisses any leftover dock pill.
    dh.set_minimized(false);
    match doc::open_doc(path) {
        Ok(content) => {
            // The doc crate owns all the document logic (blocks / pages /
            // sheets); only the slint-specific pieces — the nested shared
            // models and the first decoded unit — are built here. `mode`:
            // 0 block (DOCX) · 1 page (PPTX) · 2 sheet (XLSX).
            if !content.sheets.is_empty() {
                // XLSX: sheet view — an in-app cell grid (no rasters).
                doc_pages_replace(None);
                let first = &content.sheets[0];
                dh.set_blocks(slint::ModelRc::new(slint::VecModel::from(
                    Vec::<crate::DocBlock>::new(),
                )));
                dh.set_frame(slint::Image::default());
                dh.set_mode(2);
                dh.set_sheet_name(SharedString::from(first.name.clone()));
                dh.set_xl_rows(xl_slint_rows(first));
                dh.set_xl_total_w(first.total_width);
                dh.set_xl_truncated(first.truncated);
                dh.set_page(0);
                dh.set_page_count(content.sheets.len() as i32);
                xl_sheets_replace(Some(content.sheets));
            } else if !content.pages.is_empty() {
                // PPTX: page view. Keep the PNG bytes in the thread-local;
                // decode (cheap, bounded by the render budget) on navigation.
                xl_sheets_replace(None);
                let first = &content.pages[0];
                dh.set_blocks(slint::ModelRc::new(slint::VecModel::from(
                    Vec::<crate::DocBlock>::new(),
                )));
                dh.set_xl_rows(slint::ModelRc::new(slint::VecModel::from(
                    Vec::<crate::XlRow>::new(),
                )));
                dh.set_sheet_name(SharedString::from(""));
                dh.set_xl_total_w(0);
                dh.set_xl_truncated(false);
                dh.set_frame(crate::image::image_from_bytes(&first.png, 16384));
                dh.set_mode(1);
                dh.set_page(0);
                dh.set_page_count(content.pages.len() as i32);
                dh.set_page_ratio(first.width as f32 / (first.height as f32).max(1.0));
                doc_pages_replace(Some(content.pages));
            } else {
                // DOCX: block view.
                doc_pages_replace(None);
                xl_sheets_replace(None);
                let blocks: Vec<crate::DocBlock> = content
                    .blocks
                    .into_iter()
                    .map(|b| {
                        // The slint struct field is a nested shared model (a
                        // model of rows, each row a model of cells) — build it
                        // in two stages.
                        let slint_rows: Vec<slint::ModelRc<SharedString>> = b
                            .cells
                            .iter()
                            .map(|row| {
                                let v: Vec<SharedString> = row
                                    .iter()
                                    .map(|c| SharedString::from(c.to_string()))
                                    .collect();
                                slint::ModelRc::new(slint::VecModel::from(v))
                            })
                            .collect();
                        let slint_cells: slint::ModelRc<slint::ModelRc<SharedString>> =
                            slint::ModelRc::new(slint::VecModel::from(slint_rows));
                        // Image block: decode (capped) so the row can size itself;
                        // undecodable payloads fall back to the alt-text caption.
                        let (sm_img, iw, ih) = if b.image.is_empty() {
                            (slint::Image::default(), 0, 0)
                        } else {
                            crate::image::doc_image_from_bytes(&b.image, 640)
                        };
                        crate::DocBlock {
                            kind: b.kind.number(),
                            level: b.level,
                            text: SharedString::from(b.text),
                            cells: slint_cells,
                            cols: b.cells.iter().map(|r| r.len()).max().unwrap_or(0) as i32,
                            header_rows: b.header_rows,
                            image: sm_img,
                            img_w: iw as i32,
                            img_h: ih as i32,
                        }
                    })
                    .collect();
                dh.set_mode(0);
                dh.set_blocks(slint::ModelRc::new(slint::VecModel::from(blocks)));
                dh.set_frame(slint::Image::default());
                dh.set_sheet_name(SharedString::from(""));
                dh.set_xl_rows(slint::ModelRc::new(slint::VecModel::from(
                    Vec::<crate::XlRow>::new(),
                )));
                dh.set_xl_total_w(0);
                dh.set_xl_truncated(false);
                dh.set_page(0);
                dh.set_page_count(0);
            }
            dh.set_failed(false);
            dh.set_running(true);
            dh.set_copied(false);
            dh.set_wheel_busy(false);
            dh.set_status(SharedString::from(""));
            dh.set_title(SharedString::from(content.title));
            dh.set_format(SharedString::from(content.format));
        }
        Err(msg) => {
            doc_pages_replace(None);
            xl_sheets_replace(None);
            dh.set_blocks(slint::ModelRc::new(slint::VecModel::from(
                Vec::<crate::DocBlock>::new(),
            )));
            dh.set_frame(slint::Image::default());
            dh.set_page(0);
            dh.set_page_count(0);
            dh.set_mode(0);
            dh.set_sheet_name(SharedString::from(""));
            dh.set_xl_rows(slint::ModelRc::new(slint::VecModel::from(
                Vec::<crate::XlRow>::new(),
            )));
            dh.set_xl_total_w(0);
            dh.set_xl_truncated(false);
            dh.set_failed(true);
            dh.set_running(false);
            dh.set_copied(false);
            dh.set_wheel_busy(false);
            dh.set_status(SharedString::from(msg));
        }
    }
    let mut nav = ui.global::<crate::AppState>().get_nav_state();
    nav.global_overlay = crate::GlobalOverlayType::Doc;
    ui.global::<crate::AppState>().set_nav_state(nav);
}
