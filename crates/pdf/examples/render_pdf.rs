//! Native P1 smoke: open a real PDF with `pdf::Viewer`, render, and drive the
//! same interaction sequence the wasm `run_pdfview` smoke exercised (page
//! taps, mode switch, snap, scroll, resize) — this time with **no wasm**.
//!
//!   cargo run -p pdf --example render_pdf [path/to/doc.pdf]
//!
//! Defaults to the first PDF in `~/.chatx/files/`.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use pdf::{Mode, Step, Viewer};

fn main() {
    let pdf_path = match std::env::args().nth(1).map(PathBuf::from) {
        Some(p) if p.exists() => p,
        _ => default_pdf_path(),
    };
    if !pdf_path.exists() {
        eprintln!("missing pdf {pdf_path:?} — pass a path");
        std::process::exit(2);
    }
    run(pdf_path);
}

fn run(pdf_path: PathBuf) {
    let bytes = std::fs::read(&pdf_path).unwrap();
    let name = pdf_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("doc.pdf")
        .to_string();
    eprintln!("pdf {name} ({} bytes)", bytes.len());

    // Phone-ish canvas: 390×800 logical @ dpr 3 → 1170×2400 device.
    let width = 390u32;
    let height = 800u32;
    let dpr = 3.0;
    let v = Viewer::open_bytes(bytes, width, height, dpr).unwrap_or_else(|e| {
        eprintln!("open failed: {e}");
        std::process::exit(1);
    });
    eprintln!("opened: {} page(s)", v.page_count());

    let frame = step(&v);
    eprintln!("first frame: {}×{} ({}, width-fit)", frame.width, frame.height, name);

    // Top rows must be light content (no reserved dark toolbar band).
    let mid = (frame.width as usize) / 2;
    let top_rows: Vec<[u8; 3]> = (1..=4u32)
        .map(|r| {
            let i = ((r as usize * frame.width as usize) + mid) * 4;
            [frame.rgba[i], frame.rgba[i + 1], frame.rgba[i + 2]]
        })
        .collect();
    if top_rows.iter().any(|c| c[0] < 160 || c[1] < 160 || c[2] < 160) {
        eprintln!("FAIL: top rows dark {top_rows:?}");
        std::process::exit(1);
    }
    eprintln!("ok: top rows light content: {top_rows:?}");

    // Idle check: unchanged frame (commit-on-change; no idle repaint).
    assert!(matches!(v.step(), Step::Unchanged), "idle must not commit");
    eprintln!("ok: idle → Unchanged (commit-on-change)");

    let multi = v.page_count() > 1;

    // 1) Content-area taps page-flip horizontally: right → next, left → prev.
    {
        let cw = frame.width as i32;
        v.touch_down(cw * 3 / 4, 400);
        v.touch_up(cw * 3 / 4, 400);
        let _ = step(&v);
        if multi {
            expect_page(&v, 2, "tap right → next page");
        }
        v.touch_down(cw / 4, 400);
        v.touch_up(cw / 4, 400);
        let _ = step(&v);
        expect_page(&v, 1, "tap left → previous page");
    }

    // 2) Height-fit: page 1 fills the whole canvas.
    v.set_mode(Mode::Height);
    let height_fit = step(&v);
    assert_eq!(
        (height_fit.width, height_fit.height),
        (width * 3, height * 3),
        "height-fit canvas follows device size"
    );
    assert_eq!(v.view_state().mode, pdf::VIEW_MODE_HEIGHT);
    eprintln!("ok: height-fit view-state; page {}", v.view_state().page);

    // 3) Height mode: drag down past half a page, release → snap to nearest.
    if multi {
        let ch = (height * 3) as i32;
        v.touch_down(ch / 2, (ch * 3) / 4);
        v.touch_move(ch / 2, ch / 4); // move up → content up → next page
        v.touch_up(ch / 2, ch / 4);
        let _ = step(&v);
        eprintln!("ok: height-fit snap → page {}", v.view_state().page);
    }

    // 4) Back to width-fit + wheel/trackpad scroll (negative delta = down).
    v.set_mode(Mode::Width);
    let _ = step(&v);
    if multi {
        v.scroll(-((height * 3) as i32) / 2); // down by half a canvas
        let _ = step(&v);
        eprintln!("ok: wheel-down → page {}", v.view_state().page);
    }

    // 5) Window resize: frame must follow the new device size.
    let (rw, rh) = (900u32, 1200u32);
    v.resize(rw, rh);
    let resized = step(&v);
    assert_eq!((resized.width, resized.height), (rw, rh), "frame follows resize");
    eprintln!("ok: resize → {}×{}", resized.width, resized.height);

    // Dump a couple of frames as PPM for visual inspection.
    let out1 = dump_ppm(&height_fit, &name, "height").expect("dump height frame");
    let out2 = dump_ppm(&resized, &name, "resized").expect("dump resized frame");
    eprintln!("wrote {}", out1.display());
    eprintln!("wrote {}", out2.display());

    eprintln!("PASS: native pdf viewer rendered + controlled the fixture");
}

/// Pull the next composed frame from the viewer (drives one step).
fn step(v: &Viewer) -> pdf::Frame {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        match v.step() {
            Step::Frame(f) => return f,
            Step::Unchanged => {
                // A change was expected but none came; give a grace period then
                // fail so a regression is loud rather than silent.
                if Instant::now() >= deadline {
                    eprintln!("FAIL: expected a frame change that never arrived");
                    std::process::exit(1);
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Step::Fatal { message, .. } => {
                eprintln!("FAIL: viewer fatal: {message}");
                std::process::exit(1);
            }
        }
    }
}

fn expect_page(v: &Viewer, want: u32, what: &str) {
    let vs = v.view_state();
    let (got, pc) = (vs.page, vs.page_count);
    if got != want as i32 {
        eprintln!("FAIL: {what} → expected page {want}, got {got}");
        std::process::exit(1);
    }
    eprintln!("ok: {what} → page {got}/{pc}");
}

fn dump_ppm(f: &pdf::Frame, doc: &str, tag: &str) -> std::io::Result<PathBuf> {
    let mut rgb = Vec::with_capacity((f.width as usize * f.height as usize) * 3);
    for p in f.rgba.chunks(4) {
        if p.len() == 4 {
            rgb.extend_from_slice(&p[..3]);
        }
    }
    let safe: String = doc.chars().map(|c| if c.is_alphanumeric() { c } else { '_' }).collect();
    let (fw, fh) = (f.width, f.height);
    let out = std::env::temp_dir().join(format!("chatx_pdf_{tag}_{fw}x{fh}_{safe}.ppm"));
    let mut v = Vec::with_capacity(rgb.len() + 40);
    v.extend_from_slice(format!("P6\n{fw} {fh}\n255\n").as_bytes());
    v.extend_from_slice(&rgb);
    std::fs::write(&out, v)?;
    Ok(out)
}

fn default_pdf_path() -> PathBuf {
    if let Ok(home) = std::env::var("HOME") {
        let dir = PathBuf::from(home).join(".chatx/files");
        if let Ok(r) = std::fs::read_dir(&dir) {
            for e in r.flatten() {
                let p = e.path();
                if p.extension().and_then(|x| x.to_str()) == Some("pdf") {
                    return p;
                }
            }
        }
    }
    PathBuf::from("document.pdf")
}
