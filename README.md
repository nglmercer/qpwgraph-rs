# qpwgraph-rs

Slint desktop audio graph and control UI for native PipeWire and Windows Core
Audio backends, with ALSA Sequencer/WinMM MIDI routing, effects, metering,
patchbay persistence, and platform-specific audio relay support.

https://github.com/user-attachments/assets/d7a9b1d4-d6d3-4ef2-b0d1-4cfc2de64650

## Quick start

```bash
cargo run --release -p pw-graph-app            # native backend
cargo run --release -p pw-graph-app -- --demo  # deterministic demo backend
```

An unoptimized debug build (`cargo run` without `--release`) is dramatically
slower at graph projection and hit-testing; always compare performance in
release mode.

### Windows linker

MSVC Build Tools are required on Windows. Run Rust commands through the
workspace `xtask` so they automatically use `lld-link` when it is available on
`PATH`:

```powershell
cargo xtask run --release -p pw-graph-app
cargo xtask test --workspace --all-features --locked -- --test-threads=1
cargo xtask build --release --locked -p pw-graph-app
```

LLVM is optional. When `lld-link` is unavailable, Cargo uses the normal MSVC
linker. Explicit `CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER` and
`CARGO_TARGET_AARCH64_PC_WINDOWS_MSVC_LINKER` environment overrides always
take precedence. Arguments after `cargo xtask` are forwarded to Cargo without
reinterpretation, including `--manifest-path` commands for the nested Windows
audio-driver workspace.

Press F1 for the shortcut list. The canonical executable is always
`qpwgraph-rs`.

## Documentation

Every article lives in [`docs/`](docs). This page is the index.

### Using it

| Article | What it covers |
| --- | --- |
| [Features](docs/features.md) | What the application does, per area |
| [Running](docs/running.md) | Launching, backend selection, the CLI, keyboard |
| [Configuration and patchbay files](docs/configuration.md) | Where state lives and what is restored |
| [Effects and metering](docs/effects-and-metering.md) | The effect gallery and the three meter modes |
| [Audio relay](docs/audio-relay.md) | Hosting, pairing, endpoints, embedding the SDK |
| [Platform parity](docs/platform-parity.md) | What each backend can and cannot do, and why |

### Building and shipping

| Article | What it covers |
| --- | --- |
| [Building](docs/building.md) | Feature flags, Windows, Nix, the required checks |
| [Packaging and releases](docs/packaging.md) | AppImage, Flatpak, portable Windows ZIP |

### Internals

| Article | What it covers |
| --- | --- |
| [Workspace architecture](docs/architecture.md) | Crate layout, layering, backend namespacing |
| [The user-mode audio router](docs/audio-router.md) | The PCM routing engine: routes, mixing, conversion, meters |
| [Slint UI structure](docs/ui-components.md) | How the bridge feeds the Slint shell |
| [The keyboard contract](docs/keyboard.md) | Shortcut table, focus routing, Escape precedence |
| [Relay wire protocol, version 3](docs/relay-protocol.md) | Control and audio channels, pairing, crypto |
| [Adaptive noise reduction report](docs/adaptive-noise-reduction.md) | Why the four-band suppressor was removed |
| [Audit follow-ups](docs/audit-follow-ups.md) | Resolved audit items and regression coverage |

The relay requires an explicit PIN for first pairing. A successfully paired
identity may use its stored trusted credential on later connections; unknown
discovered peers never auto-connect. The desktop relay panel controls trusted
auto-connect and can forget a device. USB tethering is a real IP transport;
ADB forwarding is a separate explicit localhost TCP control/audio transport.

## Contributing

Before opening a pull request:

```bash
cargo fmt --all -- --check
cargo test --workspace --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

On Windows, use the corresponding `cargo xtask test`, `cargo xtask clippy`,
and other `cargo xtask <subcommand>` forms so the optional linker selection is
applied.

[Workspace architecture](docs/architecture.md) explains which crate a change
belongs in.
