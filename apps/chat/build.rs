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

    let config = slint_build::CompilerConfiguration::new()
        .with_bundled_translations(concat!(env!("CARGO_MANIFEST_DIR"), "/lang/"))
        .with_default_translation_context(slint_build::DefaultTranslationContext::None);
    slint_build::compile_with_config("ui/main.slint", config).unwrap();

    // iOS screen-share shim: when the Xcode build script compiles
    // `apps/platform/ios/Sources/ChatxScreenCapture.m` to a `.o` and points
    // `CHATX_IOS_SHIM` at it, link that object into the final `chatx`
    // executable. A bare `.o` on the link line is included in full, so the
    // `bridge` crate's references to `chatx_screen_capture_{start,stop}`
    // resolve, and the `.o`'s reference to `bridge_screen_frame_in` resolves
    // back into `bridge` (both live in the same final binary).

    println!("cargo:rerun-if-env-changed=CHATX_IOS_SHIM");
    if let Ok(shim) = std::env::var("CHATX_IOS_SHIM") {
        if shim.is_empty() {
            return;
        }
        println!("cargo:rustc-link-arg={shim}");
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
