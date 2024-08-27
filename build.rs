fn main() {
    slint_build::compile_with_config("ui/main_view.slint", slint_build::CompilerConfiguration::new()).unwrap();
}
