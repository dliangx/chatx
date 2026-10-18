//! Build script — Apple targets only.
//!
//! Compiles the Swift shims (`shim/shim.swift` camera + `shim/photos.swift`
//! album picker) into static archives that get linked into the final `chatx`
//! binary:
//!
//! - `shim/shim.swift`   — AVFoundation camera + QR + preview + still photo
//!   (`chatx_camera_*` C-ABI surface).
//! - `shim/photos.swift` — `PHPickerViewController` (iOS) / `NSOpenPanel`
//!   (macOS) picker for multi-photo album selection (`chatx_photos_*`).
//!
//! Each Swift file produces its own `.o`; both are wrapped into
//! `libshim_camera.a` (with the photo object appended) — and the static
//! library is emitted via `cargo:rustc-link-lib=static=shim_camera`, so it
//! propagates through rlib dependencies to the final chatx binary link.
//!
//! For cross-compilation (macOS → iOS), the shims are compiled with the
//! target triple + SDK the cargo build is using (via xcrun, so Xcode's
//! toolchain env is preserved).
fn main() {
    let target = std::env::var("TARGET").unwrap_or_default();
    if !target.contains("apple") {
        return;
    }

    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let shim_src = std::path::Path::new(&manifest_dir).join("shim/shim.swift");
    let photos_src = std::path::Path::new(&manifest_dir).join("shim/photos.swift");
    if !shim_src.exists() {
        eprintln!(
            "error: camera shim not found at {}; \
             the Apple build requires a Swift/AVFoundation shim",
            shim_src.display()
        );
        std::process::exit(1);
    }
    let has_photos = photos_src.exists();
    if has_photos {
        eprintln!("[camera build] photo picker shim: {}", photos_src.display());
    } else {
        eprintln!("[camera build] photo picker shim not present — album pick disabled");
    }

    let out_dir = std::env::var("OUT_DIR").unwrap();
    let out_obj = format!("{out_dir}/shim_camera.o");
    let out_obj_photos = format!("{out_dir}/shim_photos.o");
    let out_lib = format!("{out_dir}/libshim_camera.a");

    println!("cargo:rerun-if-changed={}", shim_src.display());
    if has_photos {
        println!("cargo:rerun-if-changed={}", photos_src.display());
    }

    let (swift_target, sdk_name) = match target.as_str() {
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

    let swiftc = |src: &std::path::Path, out: &str| {
        let mut cargs: Vec<String> = Vec::new();
        cargs.push("--sdk".into());
        cargs.push(sdk_name.into());
        cargs.push("swiftc".into());
        cargs.push("-target".into());
        cargs.push(swift_target.into());
        cargs.push("-parse-as-library".into());
        cargs.push("-O".into());
        cargs.push("-c".into());
        cargs.push(src.display().to_string());
        cargs.push("-o".into());
        cargs.push(out.to_string());
        cargs
    };

    let run = |args: Vec<String>| {
        let s = std::process::Command::new("xcrun")
            .args(&args)
            .status()
            .unwrap_or_else(|e| {
                eprintln!("error: failed to invoke xcrun swiftc: {e} (Xcode/CLT installed?)");
                std::process::exit(1);
            });
        if !s.success() {
            eprintln!("error: xcrun swiftc failed for: {}", args.join(" "));
            std::process::exit(1);
        }
    };

    run(swiftc(&shim_src, &out_obj));
    if has_photos {
        run(swiftc(&photos_src, &out_obj_photos));
    }

    let ar = std::process::Command::new("ar")
        .arg("rs")
        .arg(&out_lib)
        .arg(&out_obj)
        .args(if has_photos { Some(out_obj_photos) } else { None })
        .status()
        .expect("failed to invoke `ar`");
    if !ar.success() {
        eprintln!("error: `ar rs` failed while wrapping the shim static library");
        std::process::exit(1);
    }

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
    if target.contains("ios") {
        println!("cargo:rustc-link-lib=framework=PhotosUI");
    }
}
