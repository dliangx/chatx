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

    // iOS ObjC shims: when the Xcode build script compiles the
    // `apps/platform/ios/Sources/*.m` files to `.o`s and points `CHATX_IOS_SHIM`
    // at them (space-separated), link each object into the final `chatx`
    // executable. A bare `.o` on the link line is included in full, so the
    // `bridge` crate's references to `chatx_screen_capture_{start,stop}` and
    // `chatx_notify_show` resolve, and the `.o`s' references to
    // `bridge_screen_frame_in` resolve back into `bridge` (both live in the
    // same final binary).
    println!("cargo:rerun-if-env-changed=CHATX_IOS_SHIM");
    if let Ok(shim) = std::env::var("CHATX_IOS_SHIM") {
        let shims: Vec<&str> = shim.split_whitespace().collect();
        if shims.is_empty() {
            return;
        }
        for obj in &shims {
            println!("cargo:rustc-link-arg={obj}");
            println!("cargo:rerun-if-changed={obj}");
        }

        // The shims are ObjC and pull in system frameworks. A bare `.o` does
        // not auto-link its framework dependencies, so declare them here for
        // the final binary only.
        let target = std::env::var("TARGET").unwrap_or_default();
        if target.contains("apple") {
            for fw in [
                "ReplayKit",
                "CoreMedia",
                "CoreVideo",
                "Foundation",
                "UserNotifications",
            ] {
                println!("cargo:rustc-link-lib=framework={fw}");
            }
        }
    }
}
