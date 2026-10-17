fn main() {
    let config = slint_build::CompilerConfiguration::new().with_style("cosmic-dark".into());
    slint_build::compile_with_config("ui/app.slint", config).expect("Slint-Oberfläche");

    // Windows: Programmsymbol und Versionsinfo in die EXE
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/tidespeak.ico")
            .set("ProductName", "Tidespeak")
            .set("FileDescription", "Tidespeak – Sprach- und Textchat");
        res.compile().expect("Windows-Ressourcen");
    }
}
