//! Build script — Apple targets only.
//!
//! Compiles the VideoToolbox decode + encode shims (`shim/*.swift`) into a
//! static archive that gets linked into the final `chatx` binary:
//!
//! - `shim/decoder.swift` — H.264 hardware (GPU) decode via
//!   `VTDecompressionSession` (`chatx_h264_dec_*` C-ABI surface).
//! - `shim/encoder.swift` — H.264 hardware (GPU) encode via
//!   `VTCompressionSession` (`chatx_h264_enc_*` C-ABI surface).
//!
//! The objects are wrapped into `libshim_vcodec.a` and emitted via
//! `cargo:rustc-link-lib=static=shim_vcodec`, so it propagates through rlib
//! dependencies to the final binary link (the same mechanism `camera` uses for
//! its AVFoundation shim). `VideoToolbox`, `CoreMedia`, `CoreVideo`, and
//! `CoreImage` are linked as system frameworks.
//!
//! For cross-compilation (macOS → iOS), the shims are compiled with the target
//! triple + SDK the cargo build is using (via `xcrun`, preserving Xcode's
//! toolchain env).
fn main() {
    let target = std::env::var("TARGET").unwrap_or_default();
    if !target.contains("apple") {
        return;
    }

    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let dec_src = std::path::Path::new(&manifest_dir).join("shim/decoder.swift");
    let enc_src = std::path::Path::new(&manifest_dir).join("shim/encoder.swift");
    for shim_src in [&dec_src, &enc_src] {
        if !shim_src.exists() {
            eprintln!(
                "error: media video shim not found at {}; \
                 the Apple build requires this shim file",
                shim_src.display()
            );
            std::process::exit(1);
        }
    }

    let (swift_target, sdk_name) = match target.as_str() {
        // macOS 14+: the VideoToolbox decode surface we use
        // (`VTDecompressionSessionCreate` 5-param + block `DecodeFrame`) is the
        // modern, stable API.
        t if t.ends_with("aarch64-apple-darwin") => ("arm64-apple-macos14.0", "macosx"),
        t if t.ends_with("x86_64-apple-darwin") => ("x86_64-apple-macos14.0", "macosx"),
        // iOS 17+: the modern `VTDecompressionSessionCreate` /
        // `VTDecompressionSessionDecodeFrame` Swift surface is gated to
        // `iOS 17.0` / `macOS 14.0` in the SDK overlay, so the iOS target must
        // match (the macOS arm already targets macos14.0).
        "aarch64-apple-ios" => ("arm64-apple-ios17.0", "iphoneos"),
        "x86_64-apple-ios" => ("x86_64-apple-ios17.0", "iphoneos"),
        "aarch64-apple-ios-sim" => ("arm64-apple-ios17.0-simulator", "iphonesimulator"),
        "x86_64-apple-ios-sim" => ("x86_64-apple-ios17.0-simulator", "iphonesimulator"),
        other => {
            eprintln!("error: unsupported target for Swift video-decode shim: {other}");
            std::process::exit(1);
        }
    };

    let out_dir = std::env::var("OUT_DIR").unwrap();
    let out_lib = format!("{out_dir}/libshim_vcodec.a");

    for shim_src in [&dec_src, &enc_src] {
        println!("cargo:rerun-if-changed={}", shim_src.display());
    }

    let mut objs: Vec<String> = Vec::new();
    for shim_src in [&dec_src, &enc_src] {
        let base = shim_src
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let out_obj = format!("{out_dir}/shim_{base}.o");
        objs.push(out_obj.clone());
        let swiftc_args: Vec<String> = vec![
            "--sdk".into(),
            sdk_name.into(),
            "swiftc".into(),
            "-target".into(),
            swift_target.into(),
            "-parse-as-library".into(),
            "-O".into(),
            "-c".into(),
            shim_src.display().to_string(),
            "-o".into(),
            out_obj,
        ];
        let s = std::process::Command::new("xcrun")
            .args(&swiftc_args)
            .status()
            .unwrap_or_else(|e| {
                eprintln!("error: failed to invoke xcrun swiftc: {e} (Xcode/CLT installed?)");
                std::process::exit(1);
            });
        if !s.success() {
            eprintln!("error: xcrun swiftc failed for: {}", swiftc_args.join(" "));
            std::process::exit(1);
        }
    }

    let mut ar = std::process::Command::new("ar");
    ar.arg("rs").arg(&out_lib);
    for o in &objs {
        ar.arg(o);
    }
    let ar = ar.status().expect("failed to invoke `ar`");
    if !ar.success() {
        eprintln!("error: `ar rs` failed while wrapping the video shim static library");
        std::process::exit(1);
    }

    println!("cargo:rustc-link-search=native={out_dir}");
    println!("cargo:rustc-link-lib=static=shim_vcodec");
    for fw in [
        "VideoToolbox",
        "CoreMedia",
        "CoreVideo",
        "CoreImage",
        "CoreFoundation",
        "Foundation",
    ] {
        println!("cargo:rustc-link-lib=framework={fw}");
    }
}
