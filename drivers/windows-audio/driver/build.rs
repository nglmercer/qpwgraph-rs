use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use wdk_build::BuilderExt;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // This intentionally requires a real WDK/eWDK environment. The driver
    // package must never be mistaken for a normal user-mode DLL build.
    wdk_build::configure_wdk_binary_build()?;

    println!("cargo:rerun-if-env-changed=WDKContentRoot");
    println!("cargo:rerun-if-env-changed=LIBCLANG_PATH");
    println!("cargo:rerun-if-changed=src/acx_wrapper.h");
    println!("cargo:rerun-if-changed=src/acx_bridge.c");

    if env::var_os("CARGO_FEATURE_ACX").is_some() {
        generate_acx_bindings()?;
    }
    Ok(())
}

fn generate_acx_bindings() -> Result<(), Box<dyn std::error::Error>> {
    let root = env::var_os("WDKContentRoot")
        .filter(|value| !value.to_string_lossy().trim().is_empty())
        .map(PathBuf::from)
        .ok_or("the acx feature requires WDKContentRoot from an eWDK prompt")?;
    if !root.is_dir() {
        return Err(format!("WDKContentRoot is not a directory: {}", root.display()).into());
    }

    let include_root = find_include_root(&root).ok_or_else(|| {
        format!(
            "could not find a versioned WDK Include directory with km\\crt and acx.h below {}",
            root.display()
        )
    })?;
    let acx_header = find_file(&include_root, "acx.h")
        .ok_or_else(|| format!("acx.h disappeared below {}", include_root.display()))?;
    let acx_include = acx_header
        .parent()
        .ok_or("ACX header has no parent directory")?
        .to_path_buf();
    let acx_stub = find_arch_file(&root.join("Lib"), "acxstub.lib").ok_or_else(|| {
        format!(
            "could not find target-architecture acxstub.lib below {}",
            root.display()
        )
    })?;
    let wrapper = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/acx_wrapper.h");
    let out_file = PathBuf::from(env::var_os("OUT_DIR").ok_or("OUT_DIR is unavailable")?)
        .join("acx_bindings.rs");
    let wrapper_source = out_file.with_file_name("acx_static_wrappers");
    let wdk_config = wdk_build::Config::from_env_auto()?;

    // Compile against the ACX 1.1 surface supported by Windows 10 version
    // 2004. Keep the minimum framework version separate: the bridge only
    // uses DDIs common to the older ACX surface, and ACX can probe the
    // framework's available function/structure tables at runtime.
    let builder = bindgen::Builder::wdk_default(&wdk_config)?
        .header(wrapper.to_string_lossy())
        .clang_arg(format!("--include-directory={}", acx_include.display()))
        .clang_arg("--define-macro=ACX_VERSION_MAJOR=1")
        .clang_arg("--define-macro=ACX_VERSION_MINOR=1")
        .clang_arg("--define-macro=ACX_MINIMUM_VERSION_REQUIRED=0")
        .allowlist_function("^qpwgraph_acx_.*")
        .wrap_static_fns(true)
        .wrap_static_fns_path(&wrapper_source)
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()));

    let bindings = builder.generate().map_err(|error| {
        format!(
            "ACX bindings could not be generated from {}: {error}",
            include_root.display()
        )
    })?;
    bindings.write_to_file(&out_file)?;
    println!(
        "cargo:rustc-env=QPWGRAPH_ACX_BINDINGS={}",
        out_file.display()
    );

    // The wrapper functions are static inline C because they expand WDK/ACX
    // initialization macros. Bindgen emits Rust declarations for them and a
    // companion C translation unit; compile that unit with the same WDK
    // include paths and kernel-mode definitions so the Rust calls have a real
    // linkable implementation.
    let wrapper_source = wrapper_source.with_extension("c");
    let production_bridge = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/acx_bridge.c");
    let mut compiler = cc::Build::new();
    compiler
        .file(&wrapper_source)
        .file(&production_bridge)
        .warnings(false);
    for include in wdk_config.include_paths()? {
        compiler.include(include);
    }
    compiler.include(&acx_include);
    for (name, value) in wdk_config.preprocessor_definitions() {
        compiler.define(&name, value.as_deref());
    }
    compiler.define("ACX_VERSION_MAJOR", "1");
    compiler.define("ACX_VERSION_MINOR", "1");
    compiler.define("ACX_MINIMUM_VERSION_REQUIRED", "0");
    compiler.flag_if_supported("/kernel");
    compiler.compile("qpwgraph_acx_bindings");
    println!(
        "cargo:rustc-link-search=native={}",
        acx_stub
            .parent()
            .ok_or("ACX stub library has no parent directory")?
            .display()
    );
    println!("cargo:rustc-link-lib=static=acxstub");
    Ok(())
}

fn find_include_root(root: &Path) -> Option<PathBuf> {
    let include = root.join("Include");
    let mut versions = fs::read_dir(include).ok()?.flatten().collect::<Vec<_>>();
    versions.sort_by_key(|entry| entry.file_name());
    versions.into_iter().rev().find_map(|entry| {
        let candidate = entry.path();
        if !candidate.is_dir() {
            return None;
        }
        let km = candidate.join("km");
        (km.join("crt").is_dir() && find_file(&km, "acx.h").is_some()).then_some(candidate)
    })
}

fn find_file(root: &Path, wanted: &str) -> Option<PathBuf> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        let entries = fs::read_dir(&path).ok()?;
        for entry in entries.flatten() {
            let candidate = entry.path();
            if candidate.is_dir() {
                stack.push(candidate);
            } else if candidate
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.eq_ignore_ascii_case(wanted))
            {
                return Some(candidate);
            }
        }
    }
    None
}

fn find_arch_file(root: &Path, wanted: &str) -> Option<PathBuf> {
    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").ok()?;
    let target_directory = match target_arch.as_str() {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        _ => return None,
    };
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        let entries = fs::read_dir(&path).ok()?;
        for entry in entries.flatten() {
            let candidate = entry.path();
            if candidate.is_dir() {
                stack.push(candidate);
            } else if candidate
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.eq_ignore_ascii_case(wanted))
                && candidate.ancestors().any(|ancestor| {
                    ancestor
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.eq_ignore_ascii_case(target_directory))
                })
            {
                return Some(candidate);
            }
        }
    }
    None
}
