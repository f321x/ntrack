fn main() {
    let config = slint_build::CompilerConfiguration::new()
        // Material style scales best on Android and supports dark scheme.
        .with_style("material".into());
    slint_build::compile_with_config("ui/app.slint", config)
        .expect("slint compilation failed");
}
