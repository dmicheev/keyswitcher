// Build script: компилирует C-мост к Accessibility API (только на macOS)
// и линкует необходимые системные фреймворки.

fn main() {
    println!("cargo:rerun-if-changed=assets/ax_bridge.c");
    println!("cargo:rerun-if-changed=Cargo.toml");

    #[cfg(target_os = "macos")]
    {
        cc::Build::new()
            .file("assets/ax_bridge.c")
            .warnings(true)
            .compile("ax_bridge");

        // Фреймворки, нужные для TIS/Carbon/AppKit/ApplicationServices.
        println!("cargo:rustc-link-lib=framework=CoreFoundation");
        println!("cargo:rustc-link-lib=framework=Carbon");
        println!("cargo:rustc-link-lib=framework=AppKit");
        println!("cargo:rustc-link-lib=framework=ApplicationServices");
    }
}
