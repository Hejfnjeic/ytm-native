fn main() {
    let config = slint_build::CompilerConfiguration::new().with_style("fluent-dark".into());
    slint_build::compile_with_config("ui/app.slint", config).expect("compile ui/app.slint");
    // Program icon for the Windows .exe (compiled with the MinGW resource compiler).
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rerun-if-changed=assets/app.rc");
        println!("cargo:rerun-if-changed=assets/icon.ico");
        embed_resource::compile("assets/app.rc", embed_resource::NONE).manifest_required().expect("embed the program icon");
    }
}
