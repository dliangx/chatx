#![allow(dead_code)]

/// Live process RSS in MB (macOS: `ps -o rss=` reports KB).
fn rss_mb() -> u64 {
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .expect("run ps");
    let s = String::from_utf8_lossy(&out.stdout);
    s.trim().parse::<u64>().unwrap_or(u64::MAX) / 1024
}

/// Build an N-page PDF (612×792 pages, a few text lines each) with a valid xref table.
fn make_pdf(path: &std::path::Path, pages: u32) {
    let kids: Vec<String> = (0..pages).map(|i| format!("{} 0 R", 4 + i * 2)).collect();
    let pages_obj = format!("/Kids [{}] /Count {}", kids.join(" "), pages);

    let mut objs: Vec<String> = vec![
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        format!("<< /Type /Pages /Parent 1 0 R {pages_obj} >>"),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
    ];
    for i in 1..=pages {
        let body = format!(
            "<</Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources \
              << /Font << /F1 3 0 R >> >> /Contents {} 0 R>>",
            3 + i * 2
        );
        let content = format!(
            "<</Length 120>>\nstream\nBT /F1 14 Tf 72 720 Td (Page {i} of {pages}) Tj \
             0 -20 Td (Line two of page {i}, a bit longer to exercise the glyphs) Tj \
             0 -20 Td (Line three) Tj ET\nendstream"
        );
        objs.push(body);
        objs.push(content);
    }

    let mut out = String::from("%PDF-1.4\n");
    let mut offsets: Vec<usize> = Vec::new();
    for (idx, body) in objs.iter().enumerate() {
        offsets.push(out.len());
        out.push_str(&format!("{} 0 obj\n{}\nendobj\n", idx + 1, body));
    }
    let xref_pos = out.len();
    out.push_str(&format!("xref\n0 {}\n", objs.len() + 1));
    out.push_str("0000000000 65535 f \n");
    for o in &offsets {
        out.push_str(&format!("{:010} 00000 n \n", o));
    }
    out.push_str(&format!(
        "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{}\n%%EOF\n",
        objs.len() + 1,
        xref_pos
    ));
    std::fs::write(path, out).expect("write test pdf");
}

/// Mirror `lib.rs`'s pump: on a commit, the new canvas frame replaces the previous one (as on `PdfHost.frame`).
fn pump_tick(frame: &mut Option<slint::Image>) {
    if let Some(t) = chatx::pdfrs::tick() {
        if let chatx::pdfrs::Tick::Commit(c) = t {
            *frame = Some(c.img);
        }
    }
}

/// One open → scroll-the-whole-doc → close cycle, RSS-sampled at each phase.
fn cycle(path: &str, w: u32, h: u32, frame: &mut Option<slint::Image>) -> (u64, u64) {
    chatx::pdfrs::open_pdf(path, w, h, 2.0).expect("open");
    pump_tick(frame);
    let mut peak = rss_mb();
    for _ in 0..300 {
        chatx::pdfrs::scroll(-120.0, 20);
        pump_tick(frame);
        peak = peak.max(rss_mb());
    }
    // Mirror on_close: teardown the viewer, then drop the committed frame.
    *frame = None;
    chatx::pdfrs::close();
    std::thread::sleep(std::time::Duration::from_millis(400));
    (peak, rss_mb())
}

/// Full lifecycle at 1600×1000 @ dpr 2, run twice — cycle 2 must not accumulate over cycle 1.
#[test]
fn pdf_memory_lifecycle() {
    if std::env::var_os("CHATX_MEM_PROBE").is_none() {
        eprintln!("(skipped — set CHATX_MEM_PROBE=1)");
        return;
    }
    let dir = std::env::temp_dir();
    let pdf = dir.join("chatx_pdf_mem_probe.pdf");
    make_pdf(&pdf, 16);
    let p = pdf.display().to_string();

    let baseline = rss_mb();
    let mut frame = None;

    let (peak1, after1) = cycle(&p, 1600, 1000, &mut frame);
    let (peak2, after2) = cycle(&p, 1600, 1000, &mut frame);

    eprintln!(
        "rss: baseline={baseline} MB | c1 peak={peak1} after-close={after1} | \
         c2 peak={peak2} after-close={after2}",
    );
    eprintln!(
        "cycle-2 growth over cycle-1 after-close = {} MB",
        after2.saturating_sub(after1)
    );

    // No per-cycle stacking: an identical cycle 2 must not retain more RSS (a leak would grow each cycle).
    assert!(
        after2.saturating_sub(after1) < 150,
        "cycle 2 retained another {after2} MB over cycle 1's {after1} MB — accumulating leak"
    );
}

/// Same lifecycle at a full-screen Retina window (1720×1100 @ dpr 2) — closest to "maximize + close".
#[test]
fn pdf_memory_lifecycle_fullscreen() {
    if std::env::var_os("CHATX_MEM_PROBE").is_none() {
        eprintln!("(skipped — set CHATX_MEM_PROBE=1)");
        return;
    }
    let dir = std::env::temp_dir();
    let pdf = dir.join("chatx_pdf_mem_fs.pdf");
    make_pdf(&pdf, 16);
    let p = pdf.display().to_string();

    let baseline = rss_mb();
    let mut frame = None;

    let (peak, after) = cycle(&p, 1720, 1100, &mut frame);
    eprintln!(
        "fullscreen rss: baseline={baseline} MB | scroll-peak={peak} MB | after-close={after} MB",
    );

    // The whole working set (render cache + one committed frame) stays well under the old 3-image design (~640 MB).
    assert!(
        peak.saturating_sub(baseline) < 600,
        "scroll peak grew {peak} MB over the {baseline} MB baseline at full-screen size"
    );
}

/// Decisive: is retained RAM a true leak or freed-but-reusable — an identical cycle 2 must not grow RSS.
#[test]
fn pdf_memory_reuse_after_close() {
    if std::env::var_os("CHATX_MEM_PROBE").is_none() {
        eprintln!("(skipped — set CHATX_MEM_PROBE=1)");
        return;
    }
    let dir = std::env::temp_dir();
    let pdf = dir.join("chatx_pdf_mem_reuse.pdf");
    make_pdf(&pdf, 16);
    let p = pdf.display().to_string();

    let b0 = rss_mb();
    let mut frame = None;

    let (w1, a) = cycle(&p, 1600, 1000, &mut frame);
    let (w2, b1) = cycle(&p, 1600, 1000, &mut frame);
    let grow = b1.saturating_sub(a);
    eprintln!(
        "base={b0} working_set#1={w1} after_close_A={a} working_set#2={w2} B={b1} | B-A={grow} MB"
    );
    if grow > 500 {
        eprintln!("=> TRUE LEAK: close did not release the working set");
    } else {
        eprintln!(
            "=> NOT a true leak: the close freed it (cycle 2 reused it); peak RAM is the issue"
        );
    }
    // For the test-suite sanity, assert NOT a true leak.
    assert!(
        grow < 500,
        "re-building an identical working set grew RSS by {grow} MB — real reference leak"
    );
}
