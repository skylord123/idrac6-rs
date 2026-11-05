fn main() {
    println!("cargo:rerun-if-changed=assets/idrac6.ico");
    // Give the Windows executable its icon.
    #[cfg(windows)]
    {
        if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
            let mut res = winresource::WindowsResource::new();
            res.set_icon("assets/idrac6.ico");
            if let Err(e) = res.compile() {
                println!("cargo:warning=could not embed the icon: {e}");
            }
        }
    }
}
