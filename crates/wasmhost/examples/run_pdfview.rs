//! Headless P1 smoke: load the built `x.pdfview` plugin, open it with a real
//! PDF, let it render, and dump the first committed frame so sharpness can be
//! inspected. Run with:
//!
//!   cargo run -p wasmhost --example run_pdfview [path/to/app.wasm] [path/to/doc.pdf]
//!
//! Defaults to the repo's `x-t/x.pdfview/0.1.0/app.wasm` and the first PDF in
//! `~/.chatx/files/`.
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn main() {
    let mut wasm_path = default_wasm_path();
    let mut pdf_path = default_pdf_path();
    {
        let a: Vec<String> = std::env::args().skip(1).collect();
        if a.len() >= 1 {
            wasm_path = PathBuf::from(&a[0]);
        }
        if a.len() >= 2 {
            pdf_path = PathBuf::from(&a[1]);
        }
    }
    if !wasm_path.exists() {
        eprintln!("missing wasm {wasm_path:?} — run scripts/build_x.sh first");
        std::process::exit(2);
    }
    if !pdf_path.exists() {
        eprintln!("missing pdf {pdf_path:?}");
        std::process::exit(2);
    }
    let wasm = std::fs::read(&wasm_path).unwrap();
    let bytes = std::fs::read(&pdf_path).unwrap();
    let name = pdf_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("doc.pdf")
        .to_string();
    eprintln!(
        "loaded wasm {} ({} bytes), pdf {} ({} bytes)",
        wasm_path.display(),
        wasm.len(),
        pdf_path.display(),
        bytes.len(),
    );

    // Phone-ish canvas: 390×800 logical @ dpr 3 → 1170×2400 device work canvas.
    let width = 390u32;
    let height = 800u32;
    let dpr_x10 = 30u32;

    let inst = wasmhost::open_with_file(
        "x.pdfview",
        wasm,
        width,
        height,
        dpr_x10,
        Some(name),
        Some(bytes),
    )
    .unwrap_or_else(|e| {
        eprintln!("open_with_file() failed: {e}");
        std::process::exit(1);
    });

    // Drain frames until we have a committed one (render-on-demand: page 1 is
    // rasterised at device resolution on the first tick).
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut frame = None;
    while Instant::now() < deadline && frame.is_none() {
        if let Some(err) = inst.settled_error() {
            eprintln!("plugin settled with error: {err}");
            std::process::exit(1);
        }
        frame = inst.take_frame();
        std::thread::sleep(Duration::from_millis(50));
    }

    let Some(f) = frame else {
        eprintln!("no frame within deadline");
        inst.close().ok();
        std::process::exit(1);
    };
    eprintln!(
        "frame: {}×{} ({} bytes rgba), canvas requested {width}×{height} @ dpr×{}",
        f.width,
        f.height,
        f.rgba.len(),
        dpr_x10 / 10
    );

    // The normal render path must no longer reserve a top band for a
    // plugin-drawn toolbar: the very top rows are page content (white paper
    // or the light canvas fill), never the dark bar. Sample a vertical
    // centre strip and fail on a dark regression.
    let mid = (f.width as usize) / 2;
    let top_rows: Vec<[u8; 3]> = (1..=4u32)
        .map(|r| {
            let i = ((r as usize * f.width as usize) + mid) * 4;
            [f.rgba[i], f.rgba[i + 1], f.rgba[i + 2]]
        })
        .collect();
    if top_rows.iter().any(|c| c[0] < 160 || c[1] < 160 || c[2] < 160) {
        eprintln!("FAIL: top rows of the canvas are dark {top_rows:?} — plugin-drawn bar regression");
        std::process::exit(1);
    }
    eprintln!("ok: top rows are light content, no reserved plugin toolbar: {top_rows:?}");

    // --- exercise the interaction paths (all coordinates in device px) ---
    // The plugin canvas is {width}×{height} logical × dpr3 → 1170×2400 device.
    let cw = width * 3;
    let chh = height * 3;
    // The toolbar itself lives in the Slint app UI now; this headless smoke
    // drives the same host→plugin channel the UI's mode pills use — an
    // `EV_KEY_DOWN` whose `code` is a VIEW_MODE_* payload (apps/chat's
    // `xhost::push_mode`) — then checks the round-trip back through the
    // plugin's `x_view_state` report (→ `Sandbox::view_state`).
    let set_width = wasmhost::InputEvent::new(wasmhost::EV_KEY_DOWN, 0, 0, wasmhost::VIEW_MODE_WIDTH);
    let set_height = wasmhost::InputEvent::new(wasmhost::EV_KEY_DOWN, 0, 0, wasmhost::VIEW_MODE_HEIGHT);

    // 1) Initial state: width-fit by default, page 1 of N reported.
    expect_view(&inst, wasmhost::VIEW_MODE_WIDTH, Some(1), "first frame → width-fit, page 1");
    // 2) Content-area taps page-flip horizontally: right half → next page,
    //    left half → previous page.
    click(&inst, ((cw * 3) / 4) as i32, (chh / 2) as i32);
    step_frame(&inst, "tap right half → next page");
    expect_view(&inst, wasmhost::VIEW_MODE_WIDTH, Some(2), "page down → page 2");
    click(&inst, (cw / 4) as i32, (chh / 2) as i32);
    step_frame(&inst, "tap left half → previous page");
    expect_view(&inst, wasmhost::VIEW_MODE_WIDTH, Some(1), "page up → page 1");
    // 3) Host mode command → Height-fit: page 1 fills the whole canvas.
    inst.push_input(set_height);
    let height_fit = step_frame(&inst, "EV_KEY_DOWN(height) → height-fit");
    expect_view(&inst, wasmhost::VIEW_MODE_HEIGHT, Some(1), "height-fit view-state");
    // 4) Drag + release in Height mode → settle on the nearest full page.
    drag(&inst, (cw / 2) as i32, (chh / 4) as i32, ((chh * 3) / 4) as i32);
    step_frame(&inst, "height-fit drag + release → snap");
    expect_view(&inst, wasmhost::VIEW_MODE_HEIGHT, None, "snapped page still height-fit");
    // 5) Back to Width-fit (the UI's Fit-Width pill) and drag-scroll inside it.
    inst.push_input(set_width);
    step_frame(&inst, "EV_KEY_DOWN(width) → width-fit");
    drag(&inst, (cw / 2) as i32, (chh / 4) as i32, ((chh * 3) / 4) as i32);
    step_frame(&inst, "width-fit drag scroll");
    expect_view(&inst, wasmhost::VIEW_MODE_WIDTH, None, "scroll report still width-fit");
    // 6) Wheel / trackpad scroll (the UI's `scroll-event` → `EV_SCROLL`):
    //    Slint's convention is a *negative* delta-y when the user scrolls
    //    down. In Width-fit a page is ~1.4× the canvas height (width ×
    //    aspect ratio), so a half-canvas delta scrolls exactly into page 2
    //    without overshooting past it, and the inverse delta goes back.
    inst.push_input(wasmhost::InputEvent::new(wasmhost::EV_SCROLL, 0, -(chh as i32 / 2), 0));
    step_frame(&inst, "EV_SCROLL(dy=-half canvas) → wheel down");
    expect_view(&inst, wasmhost::VIEW_MODE_WIDTH, Some(2), "wheel down → page 2");
    inst.push_input(wasmhost::InputEvent::new(wasmhost::EV_SCROLL, 0, chh as i32 / 2, 0));
    step_frame(&inst, "EV_SCROLL(dy=+half canvas) → wheel up");
    expect_view(&inst, wasmhost::VIEW_MODE_WIDTH, Some(1), "wheel up → page 1");

    // Save as P6 PPM (RGB, alpha dropped) for direct visual inspection.
    let out = repo_root().join(format!("x_pdfview_smoke_{}x{}.ppm", f.width, f.height));
    let rgb = to_rgb(&f.rgba, f.width as usize, f.height as usize);
    std::fs::write(&out, make_ppm(&rgb, f.width, f.height)).unwrap();
    eprintln!("wrote {}", out.display());
    eprintln!("open it to inspect sharpness — width-fit, page 1 of the doc");

    // Also dump the height-fit (整页吸顶) frame from step 1 for inspection.
    let out_h = repo_root().join(format!("x_pdfview_height_{}x{}.ppm", height_fit.width, height_fit.height));
    let rgb_h = to_rgb(&height_fit.rgba, height_fit.width as usize, height_fit.height as usize);
    std::fs::write(&out_h, make_ppm(&rgb_h, height_fit.width, height_fit.height)).unwrap();
    eprintln!("wrote {} (height-fit, page 1 filling the whole canvas)", out_h.display());

    inst.close().unwrap_or_else(|e| {
        eprintln!("close() failed: {e}");
        std::process::exit(1);
    });
    println!("PASS: pdfview plugin rendered the fixture to a frame");
}

/// Drain a committed frame after an interaction; fail loudly if the plugin
/// died on that input. Returns the drained frame.
fn step_frame(inst: &wasmhost::PluginInstance, what: &str) -> wasmhost::Frame {
    let deadline = Instant::now() + Duration::from_millis(800);
    let mut latest = None;
    loop {
        if let Some(err) = inst.settled_error() {
            eprintln!("plugin died on '{what}': {err}");
            std::process::exit(1);
        }
        if let Some(f) = inst.take_frame() {
            latest = Some(f);
        }
        if Instant::now() >= deadline {
            match latest {
                Some(f) => {
                    eprintln!("ok: '{what}' → frame {}×{}", f.width, f.height);
                    return f;
                }
                None => {
                    eprintln!("no frame after '{what}'");
                    std::process::exit(1);
                }
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Poll the plugin's reported view-state until it settles on `want_mode`
/// (and `want_page` when given). The plugin re-reports on every changed
/// frame, so this reads a non-destructive mirror of its latest state.
///
/// When `want_page` exceeds the document's page count (e.g. a 1-page fixture
/// can't page down to 2), the page check is relaxed and the mode is still
/// asserted. Fails loudly on a dead plugin or timeout.
fn expect_view(
    inst: &wasmhost::PluginInstance,
    want_mode: i32,
    want_page: Option<u32>,
    what: &str,
) {
    let deadline = Instant::now() + Duration::from_millis(1500);
    loop {
        if let Some(err) = inst.settled_error() {
            eprintln!("plugin died before reporting '{what}': {err}");
            std::process::exit(1);
        }
        if let Some(vs) = inst.view_state() {
            let ok_mode = vs.mode as i32 == want_mode;
            let page_ok = want_page
                .map_or(true, |p| vs.page as u32 == p || vs.page_count < p);
            if ok_mode && page_ok {
                eprintln!(
                    "ok: '{what}' → view-state page {}/{} mode {}",
                    vs.page, vs.page_count, vs.mode
                );
                return;
            }
        }
        if Instant::now() >= deadline {
            let got = inst
                .view_state()
                .map(|v| format!("page {}/{} mode {}", v.page, v.page_count, v.mode))
                .unwrap_or_else(|| "no view-state yet".to_string());
            eprintln!(
                "FAIL: expected mode {want_mode} for '{what}'; last report was {got}"
            );
            std::process::exit(1);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn click(inst: &wasmhost::PluginInstance, x: i32, y: i32) {
    inst.push_input(wasmhost::InputEvent::new(wasmhost::EV_TOUCH_DOWN, x, y, 0));
    std::thread::sleep(Duration::from_millis(40));
    inst.push_input(wasmhost::InputEvent::new(wasmhost::EV_TOUCH_UP, x, y, 0));
    std::thread::sleep(Duration::from_millis(40));
}

fn drag(inst: &wasmhost::PluginInstance, x: i32, y0: i32, y1: i32) {
    inst.push_input(wasmhost::InputEvent::new(wasmhost::EV_TOUCH_DOWN, x, y0, 0));
    std::thread::sleep(Duration::from_millis(40));
    inst.push_input(wasmhost::InputEvent::new(wasmhost::EV_TOUCH_MOVE, x, y1, 0));
    std::thread::sleep(Duration::from_millis(40));
    inst.push_input(wasmhost::InputEvent::new(wasmhost::EV_TOUCH_UP, x, y1, 0));
    std::thread::sleep(Duration::from_millis(40));
}

fn to_rgb(rgba: &[u8], w: usize, h: usize) -> Vec<u8> {
    let n = (w * h).min(rgba.len() / 4);
    let mut out = Vec::with_capacity(n * 3);
    for p in 0..n {
        let i = p * 4;
        out.push(rgba[i]);
        out.push(rgba[i + 1]);
        out.push(rgba[i + 2]);
    }
    out
}

fn make_ppm(rgb: &[u8], w: u32, h: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity(rgb.len() + 40);
    v.extend_from_slice(format!("P6\n{w} {h}\n255\n").as_bytes());
    v.extend_from_slice(rgb);
    v
}

fn default_wasm_path() -> PathBuf {
    repo_root().join("x-t/x.pdfview/0.1.0/app.wasm")
}

fn default_pdf_path() -> PathBuf {
    let home = std::env::var("HOME").map(PathBuf::from).unwrap_or_default();
    let dir = home.join(".chatx/files");
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|e| e.to_str()) == Some("pdf") {
                return p;
            }
        }
    }
    dir.join("document.pdf")
}

fn repo_root() -> PathBuf {
    let here = Path::new(env!("CARGO_MANIFEST_DIR"));
    here.parent().unwrap().parent().unwrap().to_path_buf()
}
