use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let arguments: Vec<String> = std::env::args().collect();
    if arguments
        .iter()
        .any(|argument| argument == "--validate-package")
    {
        validate_package_metadata();
        return;
    }
    if arguments
        .iter()
        .any(|argument| argument == "--audit-toolchain")
    {
        audit_toolchain();
        return;
    }
    if arguments
        .iter()
        .any(|argument| argument == "--help" || argument == "-h")
    {
        print_help();
        return;
    }
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

fn print_help() {
    println!(
        "Usage: qpwgraph-audio-xtask [--validate-package | --audit-toolchain]\n\n\
         --validate-package   validate INF/manifest package markers without a WDK\n\
         --audit-toolchain    report the ACX/eWDK prerequisites and fail if any are absent\n\
         (no option)          validate the package and require WDKContentRoot"
    );
}

fn audit_toolchain() {
    validate_package_metadata();

    println!("QPWGraph Windows audio driver toolchain audit");
    println!(
        "The audit is intentionally fail-closed: a passing result only means that\n\
         the inputs needed to attempt the ACX build are present; cargo check and\n\
         the Windows endpoint/HLK tests are still required.\n"
    );

    let mut checks = Vec::new();
    let wdk_root = std::env::var_os("WDKContentRoot")
        .filter(|root| !root.to_string_lossy().trim().is_empty())
        .map(PathBuf::from);

    match wdk_root.as_ref() {
        Some(root) if root.is_dir() => {
            checks.push(Check::ok("WDKContentRoot", root.display().to_string()))
        }
        Some(root) => checks.push(Check::missing(
            "WDKContentRoot",
            format!("{} is not a directory", root.display()),
        )),
        None => checks.push(Check::missing(
            "WDKContentRoot",
            "set WDKContentRoot from an eWDK or WDK developer prompt",
        )),
    }

    if let Some(root) = wdk_root.as_ref().filter(|root| root.is_dir()) {
        let include = root.join("Include");
        match find_directory_with_suffix(&include, &["km", "crt"]) {
            Some(path) => checks.push(Check::ok("WDK KM CRT headers", path.display().to_string())),
            None => checks.push(Check::missing(
                "WDK KM CRT headers",
                format!("no Include\\<version>\\km\\crt under {}", include.display()),
            )),
        }
        match find_file(&include, "acx.h") {
            Some(path) => checks.push(Check::ok("ACX header", path.display().to_string())),
            None => checks.push(Check::missing(
                "ACX header",
                format!("acx.h was not found below {}", include.display()),
            )),
        }
        match find_arch_file(&root.join("Lib"), "acxstub.lib") {
            Some(path) => checks.push(Check::ok("ACX stub library", path.display().to_string())),
            None => checks.push(Check::missing(
                "ACX stub library",
                "target-architecture acxstub.lib was not found below the WDK Lib directory",
            )),
        }
    } else {
        checks.push(Check::missing(
            "WDK KM CRT headers",
            "not checked because WDKContentRoot is unavailable",
        ));
        checks.push(Check::missing(
            "ACX header",
            "not checked because WDKContentRoot is unavailable",
        ));
        checks.push(Check::missing(
            "ACX stub library",
            "not checked because WDKContentRoot is unavailable",
        ));
    }

    for tool in ["cl.exe", "msbuild.exe", "clang.exe"] {
        match executable_on_path(tool) {
            Some(path) => checks.push(Check::ok(tool, path)),
            None => checks.push(Check::missing(
                tool,
                "not found on PATH; use the matching VS/eWDK developer prompt",
            )),
        }
    }

    let mut missing = 0;
    for check in checks {
        if check.available {
            println!("[ok]      {:<22} {}", check.name, check.detail);
        } else {
            missing += 1;
            println!("[missing] {:<22} {}", check.name, check.detail);
        }
    }

    if missing != 0 {
        eprintln!(
            "\nACX/eWDK preflight failed: {missing} prerequisite(s) missing. \
             The bootstrap driver remains STATUS_NOT_SUPPORTED."
        );
        std::process::exit(2);
    }
    println!("\nACX/eWDK preflight passed; run cargo check for the driver next.");
}

struct Check {
    name: &'static str,
    available: bool,
    detail: String,
}

impl Check {
    fn ok(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            available: true,
            detail: detail.into(),
        }
    }

    fn missing(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            available: false,
            detail: detail.into(),
        }
    }
}

fn executable_on_path(name: &str) -> Option<String> {
    let locator = if cfg!(windows) { "where.exe" } else { "which" };
    let output = Command::new(locator).arg(name).output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(ToOwned::to_owned)
}

fn find_directory_with_suffix(root: &Path, suffix: &[&str]) -> Option<PathBuf> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        let entries = fs::read_dir(&path).ok()?;
        for entry in entries.flatten() {
            let candidate = entry.path();
            if candidate.is_dir() {
                if suffix
                    .iter()
                    .rev()
                    .zip(candidate.ancestors())
                    .all(|(part, ancestor)| {
                        ancestor
                            .file_name()
                            .and_then(|name| name.to_str())
                            .is_some_and(|name| name.eq_ignore_ascii_case(part))
                    })
                {
                    return Some(candidate);
                }
                stack.push(candidate);
            }
        }
    }
    None
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
    let target_directory = if cfg!(target_arch = "x86_64") {
        "x64"
    } else if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        return None;
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

fn validate_package_metadata() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask workspace has a parent directory");
    let package = workspace.join("package");
    let inf = package.join("qpwgraph-audio.inx");
    let driver_inf = workspace.join("driver").join("qpwgraph-audio.inx");
    let manifest = package.join("manifest.json");
    let inf_text = fs::read_to_string(&inf).unwrap_or_else(|error| {
        panic!("could not read {}: {error}", inf.display());
    });
    for required in [
        "[Version]",
        "[QPWGraph.NTamd64]",
        "AddService=qpwgraph_audio",
        "[QPWGraph_Install.NT.Wdf]",
        "[QPWGraph_Install.NT.Interfaces]",
        "AddInterface=%KSCATEGORY_RENDER%,%KSNAME_VirtualOutput%,QPWGraph_Output_Interface",
        "AddInterface=%KSCATEGORY_CAPTURE%,%KSNAME_VirtualMonitor%,QPWGraph_Monitor_Interface",
        "AddInterface=%KSCATEGORY_RENDER%,%KSNAME_RelaySink%,QPWGraph_RelaySink_Interface",
        "AddInterface=%KSCATEGORY_CAPTURE%,%KSNAME_RelayMicrophone%,QPWGraph_RelayMicrophone_Interface",
        "[QPWGraph_Output_Interface.AddReg]",
        "AddProperty=QPWGraph_Output_Interface.AddProperty",
        "[QPWGraph_Output_Interface.AddProperty]",
        "%QPWGraph_EndpointRoleGuid%,2,18,,\"app-render\"",
        "[QPWGraph_Monitor_Interface.AddReg]",
        "AddProperty=QPWGraph_Monitor_Interface.AddProperty",
        "[QPWGraph_Monitor_Interface.AddProperty]",
        "%QPWGraph_EndpointRoleGuid%,2,18,,\"app-monitor\"",
        "[QPWGraph_RelaySink_Interface.AddReg]",
        "AddProperty=QPWGraph_RelaySink_Interface.AddProperty",
        "[QPWGraph_RelaySink_Interface.AddProperty]",
        "%QPWGraph_EndpointRoleGuid%,2,18,,\"relay-render\"",
        "[QPWGraph_RelayMicrophone_Interface.AddReg]",
        "AddProperty=QPWGraph_RelayMicrophone_Interface.AddProperty",
        "[QPWGraph_RelayMicrophone_Interface.AddProperty]",
        "%QPWGraph_EndpointRoleGuid%,2,18,,\"relay-capture\"",
        "QPWGraph_EndpointRoleGuid=\"{3c8e8ef9-1f7f-4fcb-9c36-4a7e19f36d12}\"",
        "PKEY_QPWGraph_EndpointRole=\"{3c8e8ef9-1f7f-4fcb-9c36-4a7e19f36d12},2\"",
        "KmdfService=qpwgraph_audio",
    ] {
        assert!(
            inf_text.contains(required),
            "{} is missing required INF marker {required}",
            inf.display()
        );
    }
    let driver_inf_text = fs::read_to_string(&driver_inf).unwrap_or_else(|error| {
        panic!("could not read {}: {error}", driver_inf.display());
    });
    for required in [
        "[QPWGraph_Install.NT.Interfaces]",
        "AddInterface=%KSCATEGORY_RENDER%,%KSNAME_VirtualOutput%,QPWGraph_Output_Interface",
        "AddInterface=%KSCATEGORY_CAPTURE%,%KSNAME_VirtualMonitor%,QPWGraph_Monitor_Interface",
        "AddInterface=%KSCATEGORY_RENDER%,%KSNAME_RelaySink%,QPWGraph_RelaySink_Interface",
        "AddInterface=%KSCATEGORY_CAPTURE%,%KSNAME_RelayMicrophone%,QPWGraph_RelayMicrophone_Interface",
        "AddProperty=QPWGraph_Output_Interface.AddProperty",
        "[QPWGraph_Output_Interface.AddProperty]",
        "%QPWGraph_EndpointRoleGuid%,2,18,,\"app-render\"",
        "AddProperty=QPWGraph_Monitor_Interface.AddProperty",
        "[QPWGraph_Monitor_Interface.AddProperty]",
        "%QPWGraph_EndpointRoleGuid%,2,18,,\"app-monitor\"",
        "AddProperty=QPWGraph_RelaySink_Interface.AddProperty",
        "[QPWGraph_RelaySink_Interface.AddProperty]",
        "%QPWGraph_EndpointRoleGuid%,2,18,,\"relay-render\"",
        "AddProperty=QPWGraph_RelayMicrophone_Interface.AddProperty",
        "[QPWGraph_RelayMicrophone_Interface.AddProperty]",
        "%QPWGraph_EndpointRoleGuid%,2,18,,\"relay-capture\"",
        "PKEY_QPWGraph_EndpointRole=\"{3c8e8ef9-1f7f-4fcb-9c36-4a7e19f36d12},2\"",
        "QPWGraph_EndpointRoleGuid=\"{3c8e8ef9-1f7f-4fcb-9c36-4a7e19f36d12}\"",
    ] {
        assert!(
            driver_inf_text.contains(required),
            "{} is missing required INF marker {required}",
            driver_inf.display()
        );
    }
    for (path, text) in [(&inf, &inf_text), (&driver_inf, &driver_inf_text)] {
        assert!(
            !text.contains("HKR,EP\\0,%PKEY_QPWGraph_EndpointRole%"),
            "{} still publishes the semantic role through the legacy registry-only form",
            path.display()
        );
    }
    let manifest_text = fs::read_to_string(&manifest).unwrap_or_else(|error| {
        panic!("could not read {}: {error}", manifest.display());
    });
    for required in [
        "\"product\"",
        "\"driver\"",
        "\"endpoint_roles\"",
        "\"endpoint_role_property\"",
        "\"endpoint_role_property_key\"",
        "\"driver_service\"",
        "\"app-render\"",
        "\"app-monitor\"",
        "\"relay-render\"",
        "\"relay-capture\"",
        "\"implementation_status\"",
        "\"changes_default_audio_device\": false",
    ] {
        assert!(
            manifest_text.contains(required),
            "{} is missing required manifest marker {required}",
            manifest.display()
        );
    }
    assert!(
        manifest_text.contains("\"driver_service\": \"qpwgraph_audio\""),
        "{} must identify the qpwgraph_audio service",
        manifest.display()
    );
    assert!(
        manifest_text.contains("\"endpoint_role_property\": \"PKEY_QPWGraph_EndpointRole\""),
        "{} must identify the provider-owned endpoint role property",
        manifest.display()
    );
    assert!(
        manifest_text.contains(
            "\"endpoint_role_property_key\": \"{3c8e8ef9-1f7f-4fcb-9c36-4a7e19f36d12},2\""
        ),
        "{} must identify the endpoint role property key",
        manifest.display()
    );
    assert!(
        manifest_text.contains("\"implementation_status\": \"bootstrap-fail-closed\"")
            || manifest_text.contains("\"implementation_status\": \"ready\""),
        "{} has an unknown implementation_status",
        manifest.display()
    );

    let install_script = fs::read_to_string(package.join("install.ps1")).unwrap_or_else(|error| {
        panic!(
            "could not read {}: {error}",
            package.join("install.ps1").display()
        );
    });
    for required in [
        "implementation_status",
        "qpwgraph-audio.cat",
        "qpwgraph_audio.sys",
        "qpwgraph-audio-smoke",
        "--verify-roles",
        "/delete-driver",
        "Published\\s+Name",
        "driver_version",
        "SkipEndpointVerification",
    ] {
        assert!(
            install_script.contains(required),
            "{} is missing lifecycle-safety marker {required}",
            package.join("install.ps1").display()
        );
    }
    let uninstall_script =
        fs::read_to_string(package.join("uninstall.ps1")).unwrap_or_else(|error| {
            panic!(
                "could not read {}: {error}",
                package.join("uninstall.ps1").display()
            );
        });
    for required in [
        "PublishedInf",
        "--verify-absent",
        "/delete-driver",
        "SkipEndpointVerification",
    ] {
        assert!(
            uninstall_script.contains(required),
            "{} is missing lifecycle-safety marker {required}",
            package.join("uninstall.ps1").display()
        );
    }
    println!("driver package metadata validated");
}
