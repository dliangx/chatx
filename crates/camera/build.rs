//! Build script — Apple targets only.
//!
//! Compiles `shim/shim.swift` into an object file with `swiftc`, then emits
//! `cargo:rustc-link-arg=<shim.o>` and the required `-framework` flags so
//! the Swift shim (and its `@_silgen_name`-imported `chatx_rust_*`
//! references into the Rust `camera` crate) link into the *final* `chatx`
//! binary.
//!
//! We deliberately use `cargo:rustc-link-arg=` (NOT a global
//! `RUSTFLAGS=-C link-args`): the ObjC/Swift object is only pulled into
//! the final `chatx` link — not into every dependency dylib (which would
//! fail for crates that have no Swift/ObjC runtime).
//!
//! For cross-compilation from macOS to iOS (Xcode build phase), the Swift
//! shim must be compiled with the *same target triple* and SDK. The Xcode
//! phase already sets `SDK_NAME` / `ARCHS`; we honor them here so the shim
//! matches the target triple of the cargo build.
fn main() {
    let target = std::env::var("TARGET").unwrap_or_default();
    if !target.contains("apple") {
        // No Swift on Windows / Linux / Android — nothing to do.
        return;
    }

    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let shim_src = std::path::Path::new(&manifest_dir).join("shim/shim.swift");
    if !shim_src.exists() {
        eprintln!(
            "error: camera shim not found at {}; \
             the Apple build requires a Swift/AVFoundation shim",
            shim_src.display()
        );
        std::process::exit(1);
    }

    let out_dir = std::env::var("OUT_DIR").unwrap();
    let out_obj = format!("{out_dir}/shim_camera.o");
    let out_lib = format!("{out_dir}/libshim_camera.a");

    // Re-run when the source changes.
    println!("cargo:rerun-if-changed={}", shim_src.display());

    // ── pick swift target triple + matching SDK NAME ───────────────────────
    // `target` from rustc → (swift `-target` triple, xcrun SDK name).
    // The SDK *name* is passed to `xcrun --sdk <name> swiftc …` so the
    // correct SDK (macOS / iphoneos / iphonesimulator) is used to compile —
    // plain `swiftc` defaults to the macOS SDK, which fails for iOS targets.
    let (swift_target, sdk_name) = match target.as_str() {
        // AVCaptureMetadataOutput (our QR path) is macOS 13.0+, so pin the
        // deployment floor at 13.0 for macOS targets.
        t if t.ends_with("aarch64-apple-darwin") => ("arm64-apple-macos13.0", "macosx"),
        t if t.ends_with("x86_64-apple-darwin") => ("x86_64-apple-macos13.0", "macosx"),
        "aarch64-apple-ios" => ("arm64-apple-ios16.0", "iphoneos"),
        "x86_64-apple-ios" => ("x86_64-apple-ios16.0", "iphoneos"),
        "aarch64-apple-ios-sim" => ("arm64-apple-ios16.0-simulator", "iphonesimulator"),
        "x86_64-apple-ios-sim" => ("x86_64-apple-ios16.0-simulator", "iphonesimulator"),
        other => {
            eprintln!("error: unsupported target for Swift camera shim: {other}");
            std::process::exit(1);
        }
    };

    // ── compile via `xcrun --sdk <name> swiftc …` ─────────────────────────
    // Invoking through `xcrun` (rather than resolving + exec'ing the raw
    // toolchain swiftc) preserves the toolchain environment that swiftc
    // needs to locate its standard library.
    let mut cargs: Vec<String> = Vec::new();
    cargs.push("--sdk".into());
    cargs.push(sdk_name.into());
    cargs.push("swiftc".into());
    cargs.push("-target".into());
    cargs.push(swift_target.into());
    cargs.push("-parse-as-library".into());
    cargs.push("-O".into());
    cargs.push("-c".into());
    cargs.push(shim_src.display().to_string());
    cargs.push("-o".into());
    cargs.push(out_obj.clone());

    // Honour an explicit SDKROOT (a real path) if the environment provides
    // one (e.g. the Xcode build phase).
    if let Ok(sdk_root) = std::env::var("SDKROOT") {
        if !sdk_root.is_empty() {
            cargs.push("-sdk".into());
            cargs.push(sdk_root);
        }
    }

    let status = std::process::Command::new("xcrun")
        .args(&cargs)
        .status()
        .unwrap_or_else(|e| {
            eprintln!("error: failed to invoke xcrun swiftc: {e} (is Xcode/CLT installed?)");
            std::process::exit(1);
        });
    if !status.success() {
        eprintln!(
            "error: swiftc failed while compiling camera shim (target {swift_target}, sdk {sdk_name})"
        );
        std::process::exit(1);
    }

    // ── wrap in a static library ────────────────────────────────────────────
    // `cargo:rustc-link-arg=<shim.o>` does NOT propagate from an rlib dep
    // to the final binary's link (per cargo docs, `link-arg` is applied at
    // the crate's own link time; an rlib has no link time). The robust way
    // to pull native code into the final binary is a *static library*
    // referenced via `cargo:rustc-link-lib=static=...`.
    let arch = std::process::Command::new("ar")
        .arg("rs")
        .arg(&out_lib)
        .arg(&out_obj)
        .status()
        .expect("failed to invoke `ar`");
    if !arch.success() {
        eprintln!("error: `ar rs` failed while wrapping the camera shim");
        std::process::exit(1);
    }

    // ── tell the crate how to link ────────────────────────────────────────
    // `rustc-link-search` + `rustc-link-lib` propagate through rlibs to the
    // final binary's linker line. Frameworks too.
    println!("cargo:rustc-link-search=native={out_dir}");
    println!("cargo:rustc-link-lib=static=shim_camera");
    for fw in [
        "AVFoundation",
        "CoreMedia",
        "CoreVideo",
        "Foundation",
        "CoreFoundation",
    ] {
        println!("cargo:rustc-link-lib=framework={fw}");
    }
}
