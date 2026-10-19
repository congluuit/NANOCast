fn main() {
    #[cfg(windows)]
    {
        // Embeds the app icon (and version info) into nanocast.exe.
        // Regenerate the .ico with `cargo run -p icongen` after editing assets/icon.svg.
        let ico = "../../assets/nanocast.ico";
        println!("cargo:rerun-if-changed={ico}");
        // Only for a Windows target (a Windows host can also cross-check the macOS build).
        let windows_target = std::env::var("CARGO_CFG_TARGET_OS").is_ok_and(|os| os == "windows");
        if windows_target && std::path::Path::new(ico).exists() {
            let mut res = winresource::WindowsResource::new();
            res.set_icon(ico);
            res.set("ProductName", "NANOCast");
            res.set("FileDescription", "NANOCast - cast your screen to a TV on the same network");
            if let Err(e) = res.compile() {
                println!("cargo:warning=icon not embedded: {e}");
            }
        }
    }
}
