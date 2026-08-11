use slint_build::CompilerConfiguration;
use std::collections::HashMap;

fn main() {
    println!("cargo:rerun-if-changed=ui");

    let mut library_paths = HashMap::new();
    library_paths.insert(
        "slint-node-editor".to_owned(),
        "ui/slint-node-editor".into(),
    );

    let configuration = CompilerConfiguration::default().with_library_paths(library_paths);
    slint_build::compile_with_config("ui/main.slint", configuration)
        .expect("failed to compile Slint UI");
}
