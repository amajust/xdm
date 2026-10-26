fn main() {
    // build.rs itself compiles for the host, so gate on the *target* OS;
    // this keeps cross-compilation (linux -> windows) working.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        winresource::WindowsResource::new()
            .set_icon("assets/xdm-app.ico")
            .compile()
            .expect("embed windows resources");
    }
}
