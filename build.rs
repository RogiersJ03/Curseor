fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let mut resource = winresource::WindowsResource::new();
    resource.set_icon("assets/curseor.ico");
    resource.set_manifest_file("app.manifest");
    if let Err(error) = resource.compile() {
        println!("cargo:warning=failed to embed the Windows resources: {error}");
        std::process::exit(1);
    }
}
