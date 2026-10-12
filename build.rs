fn main() {
    // Einheitlicher Stil unabhängig von installierten Qt-Bibliotheken.
    let mut config = slint_build::CompilerConfiguration::new().with_style("fluent".into());
    // Element-Debuginfos nur für Debug-/Test-Builds: benötigt von den GUI-Tests
    // (i-slint-backend-testing), nicht für Release-Builds.
    if std::env::var("PROFILE").as_deref() == Ok("debug") {
        config = config.with_debug_info(true);
    }
    println!("cargo:rerun-if-env-changed=PROFILE");
    slint_build::compile_with_config("ui/main.slint", config)
        .expect("Slint-Oberfläche konnte nicht kompiliert werden");
}
