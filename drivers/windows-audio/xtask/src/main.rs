use std::fs;
use std::path::Path;

fn main() {
    let include = std::env::var_os("WDKContentRoot")
        .map(|root| Path::new(&root).join("Include"))
        .filter(|path| path.exists());
    if include.is_none() {
        eprintln!("error: WDKContentRoot is missing; run from an eWDK build environment");
        std::process::exit(2);
    }
    validate_package_metadata();
    println!("WDK environment detected");
}

fn validate_package_metadata() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask workspace has a parent directory");
    let package = workspace.join("package");
    let inf = package.join("qpwgraph-audio.inx");
    let manifest = package.join("manifest.json");
    let inf_text = fs::read_to_string(&inf).unwrap_or_else(|error| {
        panic!("could not read {}: {error}", inf.display());
    });
    for required in [
        "[Version]",
        "[QPWGraph.NTamd64]",
        "AddService=qpwgraph_audio",
        "[QPWGraph_Install.NT.Wdf]",
        "KmdfService=qpwgraph_audio",
    ] {
        assert!(
            inf_text.contains(required),
            "{} is missing required INF marker {required}",
            inf.display()
        );
    }
    let manifest_text = fs::read_to_string(&manifest).unwrap_or_else(|error| {
        panic!("could not read {}: {error}", manifest.display());
    });
    for required in [
        "\"product\"",
        "\"driver\"",
        "\"endpoint_roles\"",
        "\"implementation_status\"",
        "\"changes_default_audio_device\": false",
    ] {
        assert!(
            manifest_text.contains(required),
            "{} is missing required manifest marker {required}",
            manifest.display()
        );
    }
    println!("driver package metadata validated");
}
