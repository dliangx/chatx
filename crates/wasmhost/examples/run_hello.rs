//! Headless P0 smoke: load the built `x.hello` plugin, run it a few ticks,
//! assert a frame came back, then destroy. Run with:
//!
//!   cargo run -p wasmhost --example run_hello [path/to/app.wasm]
//!
//! (Defaults to `x-t/x.hello/0.1.0/app.wasm` at the repo root.)

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn main() {
    let mut path = default_path();
    if let Some(p) = std::env::args().nth(1) {
        path = PathBuf::from(p);
    }
    if !path.exists() {
        eprintln!("missing {path:?} — run scripts/build_x.sh first");
        std::process::exit(2);
    }
    let bytes = std::fs::read(&path).unwrap_or_else(|e| {
        eprintln!("read failed: {e}");
        std::process::exit(2);
    });
    eprintln!("loaded {} ({} bytes)", path.display(), bytes.len());

    // Canvas 640×400 @ dpr 2.
    let inst = wasmhost::open("x.hello", bytes, 640, 400, 20)
        .unwrap_or_else(|e| {
            eprintln!("open() failed: {e}");
            std::process::exit(1);
        });

    // Give the plugin thread a moment to be up; it commits a frame from init.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut got_frames = 0usize;
    let mut w = 0u32;
    let mut h = 0u32;
    while Instant::now() < deadline {
        if let Some(f) = inst.take_frame() {
            got_frames += 1;
            w = f.width;
            h = f.height;
        }
        // exercise the input queue path too
        if got_frames >= 2 {
            inst.push_input(wasmhost::InputEvent::new(wasmhost::EV_TOUCH_DOWN, 10, 10, 0));
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
        if inst.settled() {
            break;
        }
    }

    assert!(
        got_frames >= 1,
        "expected at least one committed frame (got {got_frames}); settled_error = {:?}",
        inst.settled_error()
    );
    assert_eq!((w, h), (640, 400), "frame canvas size mismatch");
    assert!(
        inst.settled_error().unwrap_or_default().is_empty(),
        "unexpected settled error: {:?}",
        inst.settled_error()
    );
    eprintln!("ok: {got_frames} frames @ {w}x{h}");

    inst.close().unwrap_or_else(|e| {
        eprintln!("close() failed: {e}");
        std::process::exit(1);
    });
    println!("PASS: hello plugin ran init → ticks → frame → destroy cleanly");
}

fn default_path() -> PathBuf {
    // crate dir = <root>/crates/wasmhost → root = ../..
    let here = Path::new(env!("CARGO_MANIFEST_DIR"));
    let root = here.parent().unwrap().parent().unwrap();
    root.join("x-t/x.hello/0.1.0/app.wasm")
}
