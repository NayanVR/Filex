fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rerun-if-changed=assets/branding/filex.ico");
        winresource::WindowsResource::new()
            .set_icon("assets/branding/filex.ico")
            .compile()
            .expect("failed to embed Filex icon in Windows executable");
    }
}
