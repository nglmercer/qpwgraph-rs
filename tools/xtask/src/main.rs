use std::env;
use std::ffi::{OsStr, OsString};
use std::process::{self, Command, Stdio};

const LINKER_ENVIRONMENTS: [&str; 2] = [
    "CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER",
    "CARGO_TARGET_AARCH64_PC_WINDOWS_MSVC_LINKER",
];
const LLD_LINK_RUSTFLAGS: &str = "-C link-arg=/force:multiple";

const MOLD_LINK_ARG: &str = "-C link-arg=-fuse-ld=mold";
const MOLD_LINKER: &str = "-C linker=clang";

/// How the tool is invoked; used by usage text, examples, and error hints.
const BIN: &str = "cargo xtask";
/// Options interpreted by xtask itself; everything else is forwarded to Cargo.
const OPT_MOLD: &str = "--mold";
const OPT_NO_MOLD: &str = "--no-mold";
const OPT_FEATURES: &str = "--features";
/// Separates Cargo options from args forwarded verbatim to the app/test harness.
const ARG_SEPARATOR: &str = "--";
const HELP_SPELLINGS: [&str; 3] = ["help", "--help", "-h"];

/// Declarative command registry: registering a command is one `CMD_*` row.
/// Parsing, usage text, examples, and Cargo forwarding derive from this table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CommandSpec {
    /// Name typed after `cargo xtask`.
    name: &'static str,
    /// Cargo subcommand invoked.
    cargo: &'static str,
    /// One-line help shown in usage.
    help: &'static str,
}

const CMD_RUN: CommandSpec = CommandSpec {
    name: "run",
    cargo: "run",
    help: "Build and run the application (args after `--` go to the app)",
};
const CMD_BUILD: CommandSpec = CommandSpec {
    name: "build",
    cargo: "build",
    help: "Build the workspace",
};
const CMD_CHECK: CommandSpec = CommandSpec {
    name: "check",
    cargo: "check",
    help: "Check the workspace without producing binaries",
};
const CMD_TEST: CommandSpec = CommandSpec {
    name: "test",
    cargo: "test",
    help: "Run tests (args after `--` go to the test harness)",
};
const CMD_CLIPPY: CommandSpec = CommandSpec {
    name: "clippy",
    cargo: "clippy",
    help: "Run Clippy lints",
};
const CMD_FMT: CommandSpec = CommandSpec {
    name: "fmt",
    cargo: "fmt",
    help: "Run rustfmt via `cargo fmt`",
};
const CMD_DOC: CommandSpec = CommandSpec {
    name: "doc",
    cargo: "doc",
    help: "Build documentation",
};
const CMD_CLEAN: CommandSpec = CommandSpec {
    name: "clean",
    cargo: "clean",
    help: "Remove build artifacts",
};

const COMMANDS: &[CommandSpec] = &[
    CMD_RUN, CMD_BUILD, CMD_CHECK, CMD_TEST, CMD_CLIPPY, CMD_FMT, CMD_DOC, CMD_CLEAN,
];

/// Options shown in usage. Own options reference the `OPT_*` vocabulary so
/// parsing and help cannot drift apart; the rest document forwarded Cargo flags.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct OptionSpec {
    flag: &'static str,
    /// Value placeholder appended after the flag, e.g. `<list>`.
    value: Option<&'static str>,
    help: &'static str,
}

const OPTIONS: &[OptionSpec] = &[
    OptionSpec {
        flag: OPT_MOLD,
        value: None,
        help: "Require mold on Linux; fail clearly if unavailable",
    },
    OptionSpec {
        flag: OPT_NO_MOLD,
        value: None,
        help: "Never use mold; use the normal system linker",
    },
    OptionSpec {
        flag: "--release",
        value: None,
        help: "Forwarded to Cargo (release mode)",
    },
    OptionSpec {
        flag: OPT_FEATURES,
        value: Some("<list>"),
        help: "Forwarded to Cargo (e.g. pipewire,relay)",
    },
    OptionSpec {
        flag: "--all-features",
        value: None,
        help: "Forwarded to Cargo",
    },
];

const OPTION_NOTES: &[&str] = &["All other options are forwarded to Cargo unchanged."];

const LINKER_LINES: &[&str] = &[
    "mold is used automatically when available to speed up development",
    "linking; otherwise the system linker is used. The wrapper prints",
    "`Linker: mold` or `Linker: system default`. mold is optional and never",
];

/// Usage examples. Each example references its registry command and the shared
/// option vocabulary; only forwarded Cargo args are literal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ExampleSpec {
    command: &'static CommandSpec,
    args: &'static [&'static str],
}

impl ExampleSpec {
    fn render(&self) -> String {
        let mut line = format!("{BIN} {}", self.command.name);
        for arg in self.args {
            line.push(' ');
            line.push_str(arg);
        }
        line
    }
}

const EXAMPLES: &[ExampleSpec] = &[
    ExampleSpec {
        command: &CMD_RUN,
        args: &["-p", "pw-graph-app"],
    },
    ExampleSpec {
        command: &CMD_RUN,
        args: &[OPT_MOLD, "-p", "pw-graph-app"],
    },
    ExampleSpec {
        command: &CMD_RUN,
        args: &[OPT_NO_MOLD, "-p", "pw-graph-app"],
    },
    ExampleSpec {
        command: &CMD_BUILD,
        args: &["--release", "-p", "pw-graph-app"],
    },
    ExampleSpec {
        command: &CMD_RUN,
        args: &["-p", "pw-graph-app", ARG_SEPARATOR, "--help"],
    },
    ExampleSpec {
        command: &CMD_TEST,
        args: &["--all-features"],
    },
    ExampleSpec {
        command: &CMD_CLIPPY,
        args: &["--all-features"],
    },
];

fn lookup_command(name: &OsStr) -> Option<&'static CommandSpec> {
    let name = name.to_str()?;
    COMMANDS.iter().find(|spec| spec.name == name)
}

fn command_names() -> String {
    COMMANDS
        .iter()
        .map(|spec| spec.name)
        .collect::<Vec<_>>()
        .join(", ")
}

fn option_label(opt: &OptionSpec) -> String {
    match opt.value {
        Some(value) => format!("{} {value}", opt.flag),
        None => opt.flag.to_string(),
    }
}

/// Renders `label  help` rows with aligned help text.
fn render_columns<L: AsRef<str>, H: AsRef<str>>(rows: &[(L, H)]) -> String {
    let width = rows
        .iter()
        .map(|(label, _)| label.as_ref().len())
        .max()
        .unwrap_or(0);
    let mut out = String::new();
    for (label, help) in rows {
        let (label, help) = (label.as_ref(), help.as_ref());
        out.push_str(&format!("  {label:<width$}  {help}\n"));
    }
    out
}

fn usage_commands() -> String {
    render_columns(
        &COMMANDS
            .iter()
            .map(|spec| (spec.name, spec.help))
            .collect::<Vec<_>>(),
    )
}

fn usage_options() -> String {
    render_columns(
        &OPTIONS
            .iter()
            .map(|opt| (option_label(opt), opt.help))
            .collect::<Vec<_>>(),
    )
}

fn usage() -> String {
    let mut out = String::from("cargo-xtask: project development CLI\n\n");
    out.push_str(&format!(
        "Usage:\n  {BIN} <command> [options] [{ARG_SEPARATOR} <args>]\n\n"
    ));
    out.push_str("Commands:\n");
    out.push_str(&usage_commands());
    out.push_str("\nOptions:\n");
    out.push_str(&usage_options());
    for note in OPTION_NOTES {
        out.push_str(&format!("  {note}\n"));
    }
    out.push_str(&format!(
        "  Arguments after `{ARG_SEPARATOR}` are forwarded verbatim.\n"
    ));
    out.push_str("\nLinker (Linux only):\n");
    for line in LINKER_LINES {
        out.push_str(&format!("  {line}\n"));
    }
    out.push_str(&format!(
        "  required: pass {OPT_NO_MOLD} to force the system linker.\n"
    ));
    out.push_str("\nExamples:\n");
    for example in EXAMPLES {
        out.push_str(&format!("  {}\n", example.render()));
    }
    out
}

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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MoldRequest {
    Auto,
    Require,
    Disable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MoldSelection {
    Mold,
    SystemDefault,
}

fn resolve_mold(
    request: MoldRequest,
    is_linux: bool,
    mold_available: bool,
) -> Result<MoldSelection, String> {
    match request {
        MoldRequest::Disable => Ok(MoldSelection::SystemDefault),
        MoldRequest::Auto => {
            if is_linux && mold_available {
                Ok(MoldSelection::Mold)
            } else {
                Ok(MoldSelection::SystemDefault)
            }
        }
        MoldRequest::Require => {
            if !is_linux {
                Err(format!(
                    "mold ({OPT_MOLD}) is only supported on Linux; using the system linker instead.\n\
                     Pass {OPT_NO_MOLD} to select the system linker explicitly."
                ))
            } else if mold_available {
                Ok(MoldSelection::Mold)
            } else {
                Err(format!(
                    "mold was required via {OPT_MOLD} but was not found on PATH (`mold --version` failed).\n\
                     Install mold or omit {OPT_MOLD} to use the system linker."
                ))
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ParsedCli {
    command: &'static CommandSpec,
    /// Cargo options before `--`; `--mold`/`--no-mold` are consumed here.
    forwarded: Vec<OsString>,
    /// Empty, or `--` plus everything after it, forwarded verbatim.
    trailing: Vec<OsString>,
    mold: MoldRequest,
}

impl ParsedCli {
    fn cargo_args(&self) -> Vec<OsString> {
        let mut args = Vec::with_capacity(self.forwarded.len() + self.trailing.len() + 1);
        args.push(OsString::from(self.command.cargo));
        args.extend(self.forwarded.iter().cloned());
        args.extend(self.trailing.iter().cloned());
        args
    }
}

#[derive(Debug)]
enum CliAction {
    Help,
    Run(ParsedCli),
}

fn parse_cli(args: &[OsString]) -> Result<CliAction, String> {
    let Some((first, rest)) = args.split_first() else {
        return Err(format!(
            "no command given. Run `{BIN} {}` for usage.",
            HELP_SPELLINGS[0]
        ));
    };
    let name = first.to_string_lossy();
    if HELP_SPELLINGS.contains(&name.as_ref()) {
        return Ok(CliAction::Help);
    }
    let Some(command) = lookup_command(first) else {
        return Err(format!(
            "unknown command '{name}'. Expected one of: {}.\nRun `{BIN} {}` for usage.",
            command_names(),
            HELP_SPELLINGS[0]
        ));
    };

    let split = rest.iter().position(|arg| arg == ARG_SEPARATOR);
    let (head, trailing) = match split {
        Some(index) => (&rest[..index], rest[index..].to_vec()),
        None => (rest, Vec::new()),
    };

    let mut mold = MoldRequest::Auto;
    let mut forwarded = Vec::new();
    let mut index = 0;
    while index < head.len() {
        let arg = &head[index];
        if arg == OPT_MOLD {
            if mold == MoldRequest::Disable {
                return Err(format!(
                    "{OPT_MOLD} and {OPT_NO_MOLD} cannot be used together."
                ));
            }
            mold = MoldRequest::Require;
        } else if arg == OPT_NO_MOLD {
            if mold == MoldRequest::Require {
                return Err(format!(
                    "{OPT_MOLD} and {OPT_NO_MOLD} cannot be used together."
                ));
            }
            mold = MoldRequest::Disable;
        } else if arg == OPT_FEATURES {
            let Some(value) = head.get(index + 1) else {
                return Err(format!(
                    "{OPT_FEATURES} requires a value, e.g. {OPT_FEATURES} pipewire,relay."
                ));
            };
            forwarded.push(arg.clone());
            forwarded.push(value.clone());
            index += 1;
        } else {
            forwarded.push(arg.clone());
        }
        index += 1;
    }

    Ok(CliAction::Run(ParsedCli {
        command,
        forwarded,
        trailing,
        mold,
    }))
}

fn tool_reports_version(tool: &str) -> bool {
    Command::new(tool)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn mold_is_available() -> bool {
    tool_reports_version("mold")
}

fn clang_is_available() -> bool {
    tool_reports_version("clang")
}

fn append_mold_flags_separated(existing: Option<OsString>, use_clang: bool) -> OsString {
    let mut value = existing;
    if use_clang {
        value = Some(append_rustflag(value, MOLD_LINKER));
    }
    append_rustflag(value, MOLD_LINK_ARG)
}

fn append_mold_flags_encoded(existing: OsString, use_clang: bool) -> OsString {
    let mut value = existing;
    if use_clang {
        value = append_encoded_rustflag(value, MOLD_LINKER);
    }
    append_encoded_rustflag(value, MOLD_LINK_ARG)
}

fn add_mold_rustflags(cargo: &mut Command, use_clang: bool) {
    // Same precedence rule as the Windows path: extend whichever RUSTFLAGS
    // source the caller already uses so Cargo's precedence order is preserved.
    // The flags are scoped to the child Cargo process; the user's global
    // Cargo configuration is never modified.
    if let Some(existing) = env::var_os("CARGO_ENCODED_RUSTFLAGS") {
        cargo.env(
            "CARGO_ENCODED_RUSTFLAGS",
            append_mold_flags_encoded(existing, use_clang),
        );
    } else if let Some(existing) = env::var_os("RUSTFLAGS") {
        cargo.env(
            "RUSTFLAGS",
            append_mold_flags_separated(Some(existing), use_clang),
        );
    } else if let Some(existing) = env::var_os("CARGO_BUILD_RUSTFLAGS") {
        cargo.env(
            "CARGO_BUILD_RUSTFLAGS",
            append_mold_flags_separated(Some(existing), use_clang),
        );
    } else {
        cargo.env("RUSTFLAGS", append_mold_flags_separated(None, use_clang));
    }
}

fn run() -> Result<i32, String> {
    let args: Vec<OsString> = env::args_os().skip(1).collect();
    let parsed = match parse_cli(&args)? {
        CliAction::Help => {
            print!("{}", usage());
            return Ok(0);
        }
        CliAction::Run(parsed) => parsed,
    };

    let is_windows = cfg!(windows);
    let is_linux = cfg!(target_os = "linux");

    // mold is a Linux-only development optimization. Probing is skipped
    // entirely off Linux and when the caller disabled mold, so non-Linux
    // linker behavior is unchanged.
    let mold_available = is_linux && parsed.mold != MoldRequest::Disable && mold_is_available();
    let mold_selection = resolve_mold(parsed.mold, is_linux, mold_available)?;
    if is_linux {
        match mold_selection {
            MoldSelection::Mold => eprintln!("Linker: mold"),
            MoldSelection::SystemDefault => eprintln!("Linker: system default"),
        }
    }

    let lld_link_available = is_windows && lld_link_is_available();
    let selections = target_selections(is_windows, lld_link_available, |name| {
        env::var_os(name).is_some()
    });

    let mut cargo = Command::new("cargo");
    cargo.args(parsed.cargo_args());

    let mut injected_lld_link = false;
    for (name, selection) in LINKER_ENVIRONMENTS.iter().zip(selections) {
        if selection == LinkerSelection::LldLink {
            cargo.env(name, "lld-link");
            injected_lld_link = true;
        }
    }

    if mold_selection == MoldSelection::Mold {
        add_mold_rustflags(&mut cargo, clang_is_available());
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

    fn os_args(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    fn parse_run(args: &[&str]) -> Result<ParsedCli, String> {
        match parse_cli(&os_args(args))? {
            CliAction::Run(parsed) => Ok(parsed),
            CliAction::Help => panic!("expected a command for {args:?}"),
        }
    }

    fn cargo_args(parsed: &ParsedCli) -> Vec<String> {
        parsed
            .cargo_args()
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

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

    #[test]
    fn every_registered_command_maps_to_its_cargo_subcommand() {
        assert!(!COMMANDS.is_empty(), "registry must declare commands");
        for spec in COMMANDS {
            let parsed = parse_run(&[spec.name]).expect("registered command parses");
            assert_eq!(parsed.command, spec);
            assert_eq!(cargo_args(&parsed), vec![spec.cargo.to_string()]);
        }
    }

    #[test]
    fn registry_names_are_unique() {
        let mut names: Vec<&str> = COMMANDS.iter().map(|spec| spec.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), COMMANDS.len());
    }

    #[test]
    fn usage_lists_every_registered_command() {
        let help = usage();
        for spec in COMMANDS {
            assert!(help.contains(spec.name), "{help}");
            assert!(help.contains(spec.help), "{help}");
        }
    }

    #[test]
    fn usage_lists_every_registered_option() {
        let help = usage();
        assert!(!OPTIONS.is_empty(), "options table must declare options");
        for opt in OPTIONS {
            assert!(help.contains(&option_label(opt)), "{help}");
            assert!(help.contains(opt.help), "{help}");
        }
    }

    #[test]
    fn examples_reference_registered_commands() {
        assert!(!EXAMPLES.is_empty(), "examples table must declare examples");
        for example in EXAMPLES {
            assert!(
                COMMANDS.contains(example.command),
                "example references an unregistered command: {example:?}"
            );
        }
    }

    #[test]
    fn usage_lists_every_example() {
        let help = usage();
        for example in EXAMPLES {
            assert!(help.contains(&example.render()), "{help}");
        }
    }

    #[test]
    fn missing_command_is_an_error() {
        let error = parse_cli(&[]).expect_err("no command must fail");
        assert!(error.contains("no command"), "{error}");
    }

    #[test]
    fn unknown_command_is_an_error() {
        let error = parse_run(&["frobnicate"]).expect_err("unknown command must fail");
        assert!(error.contains("unknown command 'frobnicate'"), "{error}");
        assert!(error.contains("cargo xtask help"), "{error}");
    }

    #[test]
    fn help_spellings_request_usage() {
        for spelling in HELP_SPELLINGS {
            assert!(
                matches!(parse_cli(&os_args(&[spelling])), Ok(CliAction::Help)),
                "{spelling} should request usage",
            );
        }
    }

    #[test]
    fn mold_requests_are_stripped_from_cargo_args() {
        let parsed = parse_run(&["run", "--mold"]).expect("run --mold parses");
        assert_eq!(parsed.mold, MoldRequest::Require);
        assert_eq!(cargo_args(&parsed), vec!["run".to_string()]);

        let parsed = parse_run(&["test", "--no-mold"]).expect("test --no-mold parses");
        assert_eq!(parsed.mold, MoldRequest::Disable);
        assert_eq!(cargo_args(&parsed), vec!["test".to_string()]);
    }

    #[test]
    fn mold_and_no_mold_conflict() {
        let error = parse_run(&["run", "--mold", "--no-mold"]).expect_err("conflict must fail");
        assert!(error.contains("--mold and --no-mold"), "{error}");
    }

    #[test]
    fn release_is_forwarded_to_cargo() {
        let parsed = parse_run(&["build", "--release"]).expect("build --release parses");
        assert_eq!(cargo_args(&parsed), vec!["build", "--release"]);

        let parsed = parse_run(&["run", "--release"]).expect("run --release parses");
        assert_eq!(cargo_args(&parsed), vec!["run", "--release"]);
    }

    #[test]
    fn features_are_forwarded_to_cargo() {
        let parsed = parse_run(&["run", "--features", "pipewire,relay"]).expect("features parse");
        assert_eq!(
            cargo_args(&parsed),
            vec!["run", "--features", "pipewire,relay"]
        );

        let parsed = parse_run(&["build", "--all-features"]).expect("all-features parses");
        assert_eq!(cargo_args(&parsed), vec!["build", "--all-features"]);

        let parsed = parse_run(&["test", "--all-features"]).expect("test features parse");
        assert_eq!(cargo_args(&parsed), vec!["test", "--all-features"]);

        let parsed =
            parse_run(&["build", "--features=pipewire,relay"]).expect("joined features parse");
        assert_eq!(
            cargo_args(&parsed),
            vec!["build", "--features=pipewire,relay"]
        );
    }

    #[test]
    fn features_without_a_value_is_an_error() {
        let error = parse_run(&["build", "--features"]).expect_err("missing value must fail");
        assert!(error.contains("--features requires a value"), "{error}");
    }

    #[test]
    fn arguments_after_dashdash_are_forwarded_verbatim() {
        let parsed = parse_run(&["run", "--", "--help"]).expect("trailing args parse");
        assert_eq!(parsed.mold, MoldRequest::Auto);
        assert_eq!(cargo_args(&parsed), vec!["run", "--", "--help"]);

        // xtask options after `--` belong to the application, not to xtask.
        let parsed =
            parse_run(&["run", "--mold", "--", "--mold"]).expect("mold plus trailing parses");
        assert_eq!(parsed.mold, MoldRequest::Require);
        assert_eq!(cargo_args(&parsed), vec!["run", "--", "--mold"]);
    }

    #[test]
    fn other_cargo_options_pass_through_untouched() {
        let parsed = parse_run(&[
            "test",
            "--workspace",
            "--locked",
            "-p",
            "pw-graph-app",
            "--",
            "--test-threads=1",
        ])
        .expect("cargo options parse");
        assert_eq!(
            cargo_args(&parsed),
            vec![
                "test",
                "--workspace",
                "--locked",
                "-p",
                "pw-graph-app",
                "--",
                "--test-threads=1",
            ]
        );
    }

    #[test]
    fn mold_available_selects_mold() {
        assert_eq!(
            resolve_mold(MoldRequest::Auto, true, true),
            Ok(MoldSelection::Mold)
        );
    }

    #[test]
    fn mold_unavailable_falls_back_to_the_system_linker() {
        assert_eq!(
            resolve_mold(MoldRequest::Auto, true, false),
            Ok(MoldSelection::SystemDefault)
        );
    }

    #[test]
    fn required_mold_unavailable_is_an_error() {
        let error = resolve_mold(MoldRequest::Require, true, false).expect_err("must fail");
        assert!(error.contains("--mold"), "{error}");
    }

    #[test]
    fn no_mold_disables_mold_even_when_available() {
        assert_eq!(
            resolve_mold(MoldRequest::Disable, true, true),
            Ok(MoldSelection::SystemDefault)
        );
    }

    #[test]
    fn non_linux_keeps_the_system_linker() {
        assert_eq!(
            resolve_mold(MoldRequest::Auto, false, true),
            Ok(MoldSelection::SystemDefault)
        );
        assert_eq!(
            resolve_mold(MoldRequest::Disable, false, true),
            Ok(MoldSelection::SystemDefault)
        );
    }

    #[test]
    fn required_mold_on_non_linux_is_an_error() {
        let error = resolve_mold(MoldRequest::Require, false, true).expect_err("must fail");
        assert!(error.contains("only supported on Linux"), "{error}");
    }

    #[test]
    fn mold_rustflags_preserve_existing_flags() {
        assert_eq!(
            append_mold_flags_separated(Some(OsString::from("-D warnings")), true)
                .to_string_lossy(),
            "-D warnings -C linker=clang -C link-arg=-fuse-ld=mold"
        );
        assert_eq!(
            append_mold_flags_separated(None, false).to_string_lossy(),
            "-C link-arg=-fuse-ld=mold"
        );
    }

    #[test]
    fn mold_encoded_rustflags_keep_the_cargo_separator() {
        assert_eq!(
            append_mold_flags_encoded(OsString::from("-D\u{1f}warnings"), true).to_string_lossy(),
            "-D\u{1f}warnings\u{1f}-C\u{1f}linker=clang\u{1f}-C\u{1f}link-arg=-fuse-ld=mold"
        );
    }
}
