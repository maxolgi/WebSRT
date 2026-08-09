/// Ensure `web/dist/` exists so `rust-embed`'s `#[derive(Embed)]` doesn't fail
/// at compile time when the web UI hasn't been built yet. The directory will
/// be empty (all embedded assets return `None`) — the gateway still compiles
/// and runs; it just returns 404 for web requests until `./build.sh web build`
/// populates `dist/`.
fn main() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let dist = std::path::Path::new(&manifest_dir).join("../../web/dist");
    if !dist.exists() {
        let _ = std::fs::create_dir_all(&dist);
        let _ = std::fs::write(dist.join(".gitkeep"), "");
        println!(
            "cargo:warning=web/dist/ was empty; run './build.sh web build' to build the web UI"
        );
    }
    println!("cargo:rerun-if-changed=../../web/dist");

    // Embed the icon into the Windows executable (shown in Explorer/taskbar).
    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        res.set("FileDescription", "WebSRT Gateway");
        if let Err(e) = res.compile() {
            println!("cargo:warning=failed to embed Windows icon resource: {e}");
        }
    }
    println!("cargo:rerun-if-changed=assets/icon.ico");
}
