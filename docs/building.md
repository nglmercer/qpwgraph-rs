# Building

Native builds per platform, the feature flags that select a backend, Nix, and
the checks that have to pass before a change lands.

## Feature defaults

On Linux, the default build enables PipeWire, ALSA MIDI, relay, and tray
support. On Windows, PipeWire and ALSA are not built or required; the native
backends use Windows Core Audio/WASAPI through a dedicated COM worker thread
and WinMM for MIDI.

```bash
cargo build --release -p pw-graph-app
cargo build --release -p pw-graph-app --no-default-features
cargo build --release -p pw-graph-app --no-default-features --features pipewire
cargo build --release -p pw-graph-app --no-default-features --features alsa
```

The relay is enabled by the default `relay` feature and can be selected
explicitly:

```bash
cargo run -p pw-graph-app --features relay
cargo run -p pw-graph-app --no-default-features --features pipewire,relay
```

### Which combinations are real

A feature that names a Linux implementation is inert on Windows and vice
versa, so the combinations worth building are not the full power set. CI
builds exactly these, and each one is a state a user can ship:

| Platform | Combination | What it gives |
| --- | --- | --- |
| Linux | none | demo backend only |
| Linux | `pipewire` | native audio graph |
| Linux | `alsa` | native MIDI graph |
| Linux | `pipewire,alsa` | both native graphs |
| Linux | `pipewire,relay` | native audio plus the relay |
| Linux | default (`pipewire,alsa,relay,tray`) | everything |
| Windows | none | demo backend only |
| Windows | `relay` | relay over WASAPI endpoints |
| Windows | `tray` | notification-area icon |
| Windows | `relay,tray` | both |
| Windows | default | everything; `pipewire` and `alsa` are inert here |

Windows Core Audio, the user-mode router, and WinMM MIDI are not behind
features: they are compiled whenever the target is Windows. Adding a feature
for them would only create a state in which the application has no way to see
the machine's audio.

## Windows

On Windows, the standard MSVC commands are:

```powershell
cargo run -p pw-graph-app -- --demo
cargo build --release --locked -p pw-graph-app
```

The optional virtual-audio driver is not part of those portable commands. It
has its own workspace and requires an eWDK/WDK developer prompt with KMDF/ACX
headers plus a released LLVM 17--21 toolchain for bindgen. LLVM 22 currently
produces invalid WDK layout bindings. Run driver commands from the nested
workspace so its static-CRT configuration is loaded:

```powershell
Push-Location drivers/windows-audio
$env:LIBCLANG_PATH = 'C:\LLVM21\bin' # adjust to your LLVM 17--21 installation
$env:Path = "$env:LIBCLANG_PATH;$env:Path"
cargo test -p qpwgraph-audio-core --locked
cargo run -p qpwgraph-audio-xtask --locked -- --audit-toolchain
cargo check -p qpwgraph-audio --features acx --locked
cargo make
Pop-Location
```

The deterministic process-loopback target is a normal user-mode build and
does not require the driver:

```powershell
cargo run -p windows-audio-test-tone --release -- --duration-ms 30000
```

On a normal SDK-only machine the driver build fails closed with a missing WDK
header error. Do not substitute a user-mode DLL for the kernel package.

## Nix

```bash
nix develop
nix build
nix run
nix flake check
```

`nix run` launches the Slint application and the default package installs the
`qpwgraph-rs` executable.

## Development checks

```bash
cargo fmt --all -- --check
cargo test --workspace --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo build --release --locked
```

CI sets `RUSTFLAGS=-D warnings` for every job, so a plain `cargo check` on a
feature subset fails on a warning rather than printing one and passing. To
reproduce that locally, set the same variable:

```bash
RUSTFLAGS="-D warnings" cargo check -p pw-graph-app --locked --no-default-features
```

Registry dependencies are unaffected — cargo caps their lints — so this covers
the workspace and the vendored path crates, which is the code this repository
is responsible for.

## Related

- [Workspace architecture](architecture.md) — what each crate owns.
- [Packaging and releases](packaging.md) — producing distributable artifacts.
