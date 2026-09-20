use std::env;
use std::ffi::OsString;
use std::process::{self, Command, Stdio};

const LINKER_ENVIRONMENTS: [&str; 2] = [
    "CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER",
    "CARGO_TARGET_AARCH64_PC_WINDOWS_MSVC_LINKER",
];
const LLD_LINK_RUSTFLAGS: &str = "-C link-arg=/force:multiple";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LinkerSelection {
    PreserveOverride,
    LldLink,
    CargoDefault,
}

fn select_linker(
    is_windows: bool,
    lld_link_available: bool,
    explicit_override: bool,
) -> LinkerSelection {
    if !is_windows {
        LinkerSelection::CargoDefault
    } else if explicit_override {
        LinkerSelection::PreserveOverride
    } else if lld_link_available {
        LinkerSelection::LldLink
    } else {
        LinkerSelection::CargoDefault
    }
}

fn target_selections<F>(
    is_windows: bool,
    lld_link_available: bool,
    mut has_override: F,
) -> [LinkerSelection; 2]
where
    F: FnMut(&str) -> bool,
{
    LINKER_ENVIRONMENTS.map(|name| {
        select_linker(
            is_windows,
            lld_link_available,
            is_windows && has_override(name),
        )
    })
}

fn lld_link_is_available() -> bool {
    Command::new("lld-link.exe")
        .arg("/?")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

fn append_rustflag(existing: Option<OsString>, flag: &str) -> OsString {
    let mut value = existing.unwrap_or_default();
    if !value.is_empty() {
        value.push(" ");
    }
    value.push(flag);
    value
}

fn append_encoded_rustflag(existing: OsString, flag: &str) -> OsString {
    let mut value = existing;
    value.push("\u{1f}");
    value.push("-C");
    value.push("\u{1f}");
    value.push(flag.strip_prefix("-C ").unwrap_or(flag));
    value
}

fn add_lld_link_compatibility_flags(cargo: &mut Command) {
    // Cargo gives encoded/global RUSTFLAGS precedence over target-specific
    // RUSTFLAGS. Preserve whichever source the caller supplied and append the
    // lld-only compatibility option there; otherwise append it to each target
    // setting that xtask may inject. This flag is deliberately scoped to the
    // child Cargo process and is never written to the user's environment.
    if let Some(existing) = env::var_os("CARGO_ENCODED_RUSTFLAGS") {
        cargo.env(
            "CARGO_ENCODED_RUSTFLAGS",
            append_encoded_rustflag(existing, LLD_LINK_RUSTFLAGS),
        );
    } else if let Some(existing) = env::var_os("RUSTFLAGS") {
        cargo.env(
            "RUSTFLAGS",
            append_rustflag(Some(existing), LLD_LINK_RUSTFLAGS),
        );
    } else if let Some(existing) = env::var_os("CARGO_BUILD_RUSTFLAGS") {
        cargo.env(
            "CARGO_BUILD_RUSTFLAGS",
            append_rustflag(Some(existing), LLD_LINK_RUSTFLAGS),
        );
    } else {
        for target in LINKER_ENVIRONMENTS {
            let rustflags = target.replace("_LINKER", "_RUSTFLAGS");
            if env::var_os(target).is_none() {
                cargo.env(rustflags, LLD_LINK_RUSTFLAGS);
            }
        }
    }
}

fn run() -> Result<i32, String> {
    let is_windows = cfg!(windows);
    let lld_link_available = is_windows && lld_link_is_available();
    let selections = target_selections(is_windows, lld_link_available, |name| {
        env::var_os(name).is_some()
    });

    let mut cargo = Command::new("cargo");
    cargo.args(env::args_os().skip(1));

    let mut injected_lld_link = false;
    for (name, selection) in LINKER_ENVIRONMENTS.iter().zip(selections) {
        if selection == LinkerSelection::LldLink {
            cargo.env(name, "lld-link");
            injected_lld_link = true;
        }
    }

    if injected_lld_link {
        // Skia's bundled ICU and windows-sys' raw ICU imports intentionally
        // provide the same symbols. MSVC accepts that combination, while
        // lld-link requires the explicit multiple-definition compatibility
        // switch. It is only added when this wrapper selected lld-link.
        add_lld_link_compatibility_flags(&mut cargo);
        eprintln!("cargo-xtask: using lld-link");
    } else if is_windows
        && !lld_link_available
        && selections.contains(&LinkerSelection::CargoDefault)
    {
        eprintln!("cargo-xtask: lld-link unavailable; using Cargo default linker");
    }

    cargo
        .status()
        .map(|status| status.code().unwrap_or(1))
        .map_err(|error| format!("could not start Cargo: {error}"))
}

fn main() {
    let exit_code = run().unwrap_or_else(|error| {
        eprintln!("cargo-xtask: {error}");
        1
    });
    process::exit(exit_code);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_override_is_preserved() {
        assert_eq!(
            select_linker(true, true, true),
            LinkerSelection::PreserveOverride
        );
    }

    #[test]
    fn windows_uses_lld_link_when_available() {
        assert_eq!(select_linker(true, true, false), LinkerSelection::LldLink);
    }

    #[test]
    fn windows_leaves_linker_unset_when_lld_link_is_unavailable() {
        assert_eq!(
            select_linker(true, false, false),
            LinkerSelection::CargoDefault
        );
    }

    #[test]
    fn non_windows_leaves_linker_unset() {
        assert_eq!(
            select_linker(false, true, false),
            LinkerSelection::CargoDefault
        );
    }

    #[test]
    fn x86_64_override_does_not_block_aarch64_lld_link() {
        let selections = target_selections(true, true, |name| {
            name == "CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER"
        });
        assert_eq!(
            selections,
            [LinkerSelection::PreserveOverride, LinkerSelection::LldLink]
        );
    }

    #[test]
    fn aarch64_override_does_not_block_x86_64_lld_link() {
        let selections = target_selections(true, true, |name| {
            name == "CARGO_TARGET_AARCH64_PC_WINDOWS_MSVC_LINKER"
        });
        assert_eq!(
            selections,
            [LinkerSelection::LldLink, LinkerSelection::PreserveOverride]
        );
    }

    #[test]
    fn rustflags_are_appended_without_replacing_existing_flags() {
        assert_eq!(
            append_rustflag(Some(OsString::from("-D warnings")), LLD_LINK_RUSTFLAGS)
                .to_string_lossy(),
            "-D warnings -C link-arg=/force:multiple"
        );
    }

    #[test]
    fn encoded_rustflags_keep_the_cargo_separator() {
        assert_eq!(
            append_encoded_rustflag(OsString::from("-D\u{1f}warnings"), LLD_LINK_RUSTFLAGS,)
                .to_string_lossy(),
            "-D\u{1f}warnings\u{1f}-C\u{1f}link-arg=/force:multiple"
        );
    }
}
