//! `x.hello` — the P0 smoke-test mini-program.
//!
//! Proves the full loop: `init → tick → x_frame_commit → host Image`,
//! including plugin `std` alloc/threads and stdout logging through the
//! hosted WASI surface. A bouncing brand-colored box, one static headline,
//! and a live tick counter — no document parsing involved.

use std::cell::RefCell;

use appsdk::{with_ctx, rgba};

const BG: u32 = rgba(16, 18, 24, 255);
const BOX: u32 = rgba(74, 124, 255, 255);
const FG: u32 = rgba(238, 240, 248, 255);
const MUTED: u32 = rgba(140, 146, 168, 255);

struct HelloApp {
    box_x: f32,
    box_y: f32,
    vx: f32,
    vy: f32,
    ticks: u64,
}

thread_local! {
    static APP: RefCell<Option<HelloApp>> = const { RefCell::new(None) };
}

#[unsafe(no_mangle)]
pub extern "C" fn init(width: u32, height: u32, dpr_x10: u32) -> i32 {
    let dpr = dpr_x10 as f32 / 10.0;
    appsdk::begin(width, height, dpr);
    let w = width as f32;
    let h = height as f32;
    let size = (w.min(h) * 0.09).max(32.0);
    APP.with(|a| {
        *a.borrow_mut() = Some(HelloApp {
            box_x: w * 0.12,
            box_y: (h * 0.22).max(90.0),
            vx: 2.4,
            vy: 1.9,
            ticks: 0,
        });
    });
    draw_frame(w * 0.12, (h * 0.22).max(90.0), size, 0, w, h);
    appsdk::log(&format!("x.hello: init ok ({width}x{height} dpr x{dpr})"));
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn tick(dt_ms: i64) -> i32 {
    let dt = dt_ms as f32;
    let (w, h) = canvas_wh();
    let snap = APP.with(|a| {
        let mut guard = a.borrow_mut();
        let app = guard.as_mut()?;
        let size = (w.min(h) * 0.09).max(32.0);
        app.ticks += 1;
        // motion in px per frame (dt-normalized so any tick cadence looks alike)
        app.box_x += app.vx * dt * 0.06;
        app.box_y += app.vy * dt * 0.06;
        if app.box_x < 8.0 {
            app.box_x = 8.0;
            app.vx = app.vx.abs();
        }
        if app.box_x > (w - size - 8.0).max(8.0) {
            app.box_x = (w - size - 8.0).max(8.0);
            app.vx = -app.vx.abs();
        }
        if app.box_y < 90.0 {
            app.box_y = 90.0;
            app.vy = app.vy.abs();
        }
        if app.box_y > (h - size - 56.0).max(90.0) {
            app.box_y = (h - size - 56.0).max(90.0);
            app.vy = -app.vy.abs();
        }
        Some((app.box_x, app.box_y, size, app.ticks))
    });

    if let Some((bx, by, size, ticks)) = snap {
        draw_frame(bx, by, size, ticks, w, h);
    }
    // Drain pushed input (the host forwards window drag-resizes as
    // `EV_RESIZE`): resize the canvas buffer to the new window size and
    // re-draw, so the demo fills the window like pdfview does and never
    // letterboxes.
    let mut resized = false;
    while let Some(ev) = appsdk::poll_input() {
        if ev.kind == appsdk::EV_RESIZE {
            let w = ev.x.max(1) as u32;
            let h = ev.y.max(1) as u32;
            let cur = canvas_wh();
            if w as f32 != cur.0 || h as f32 != cur.1 {
                let dpr = with_ctx(|ctx| ctx.dpr).unwrap_or(1.0);
                appsdk::begin(w, h, dpr);
                resized = true;
            }
        }
    }
    if resized {
        let (w, h) = canvas_wh();
        let size = (w.min(h) * 0.09).max(32.0);
        let snap = APP.with(|a| {
            let mut guard = a.borrow_mut();
            let app = guard.as_mut()?;
            // Keep the box inside the (possibly smaller) new canvas.
            app.box_x = app.box_x.clamp(8.0, (w - size - 8.0).max(8.0));
            app.box_y = app.box_y.clamp(90.0, (h - size - 56.0).max(90.0));
            Some((app.box_x, app.box_y, size, app.ticks))
        });
        if let Some((bx, by, size, ticks)) = snap {
            draw_frame(bx, by, size, ticks, w, h);
        }
    }
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn destroy() -> i32 {
    appsdk::log("x.hello: destroy");
    APP.with(|a| *a.borrow_mut() = None);
    0
}

/// Canvas size via CTX (a different thread_local than APP — no reentry).
fn canvas_wh() -> (f32, f32) {
    let mut out = (800f32, 600f32);
    with_ctx(|ctx| {
        out = (ctx.width as f32, ctx.height as f32);
    });
    out
}

fn draw_frame(bx: f32, by: f32, size: f32, ticks: u64, w: f32, h: f32) {
    with_ctx(|ctx| {
        ctx.clear(BG);
        ctx.fill_text(24, 24, 3, "Hello, ChatX!", FG);
        ctx.fill_text(24, 62, 2, "x.hello - the wasm sandbox is alive", MUTED);
        let s = size as i32;
        ctx.fill_rect(bx as i32, by as i32, s, s, BOX);
        let line = format!("tick {ticks}   |   canvas {w:.0}x{h:.0}");
        ctx.fill_text(24, (h as i32) - 40, 2, &line, MUTED);
        ctx.commit();
    });
}
