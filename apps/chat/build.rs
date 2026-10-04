fn main() {
    // Windows: embed icon + version info at compile time (no rcedit needed).
    #[cfg(windows)]
    {
        let icon = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("app-icon.ico");
        let mut res = winres::WindowsResource::new();
        res.set_icon(icon.to_str().unwrap());
        res.set("ProductName", "Chatx");
        res.set("FileDescription", "Chatx Desktop");
        res.set("LegalCopyright", "Chatx");
        res.compile().expect("failed to compile Windows resources");
    }

    slint_build::compile("ui/main.slint").unwrap();

    // iOS screen-share shim: when the Xcode build script compiles
    // `apps/platform/ios/Sources/ChatxScreenCapture.m` to a `.o` and points
    // `CHATX_IOS_SHIM` at it, link that object into the final `chatx`
    // executable. A bare `.o` on the link line is included in full, so the
    // `bridge` crate's references to `chatx_screen_capture_{start,stop}`
    // resolve, and the `.o`'s reference to `bridge_screen_frame_in` resolves
    // back into `bridge` (both live in the same final binary).
    //
    // We emit `cargo:rustc-link-arg` (NOT `RUSTFLAGS=-C link-args`) so the
    // object is added ONLY to the final chatx link — not to every dependency
    // dylib (which would fail because those don't link the ObjC runtime).
    if let Ok(shim) = std::env::var("CHATX_IOS_SHIM") {
        if shim.is_empty() {
            return;
        }
        println!("cargo:rustc-link-arg={shim}");
        println!("cargo:rerun-if-env-changed=CHATX_IOS_SHIM");
        println!("cargo:rerun-if-changed={shim}");

        // The shim is ObjC++/ObjC and pulls in system frameworks. A bare `.o`
        // does not auto-link its framework dependencies, so declare them here
        // for the final binary only.
        let target = std::env::var("TARGET").unwrap_or_default();
        if target.contains("apple") {
            for fw in ["ReplayKit", "CoreMedia", "CoreVideo", "Foundation"] {
                println!("cargo:rustc-link-lib=framework={fw}");
            }
        }
    }
}
