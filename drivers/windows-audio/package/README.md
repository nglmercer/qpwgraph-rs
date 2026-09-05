# QPWGraph Windows virtual-audio package

This is a nested Rust workspace so normal application builds never acquire a
WDK dependency. Use an eWDK prompt with KMDF 1.33 and LLVM available:

```powershell
cargo test -p qpwgraph-audio-core
cargo run -p qpwgraph-audio-xtask -- --validate-package
cargo run -p qpwgraph-audio-xtask -- --audit-toolchain
cargo run -p qpwgraph-audio-xtask
cargo make --cwd drivers/windows-audio
cargo run -p qpwgraph-audio-smoke -- --list
```

The default driver build intentionally returns `STATUS_NOT_SUPPORTED` from
device-add. The opt-in `acx` build now contains the ACX app and relay endpoint
transactions (device, circuits, pins, format, RT packet timing, and two
independent Rust bounded PCM cables), but it is not installable until an eWDK
build and a test-signed Windows validation pass prove that path. This default fail-closed
state prevents an unvalidated development binary from being confused with a
successful release driver.
The installer checks `manifest.json` and refuses packages whose
`implementation_status` is not `ready`.

The install metadata is `package/qpwgraph-audio.inx`; the copy beside the
driver source is retained as a template for driver-local builds.

The package must never set a Windows default audio device. Test signing is for
development machines only; public packages require Microsoft signing and
Secure Boot validation.

The four endpoint roles are published as provider-owned custom interface
properties with INF `AddProperty` sections: `app-render` for Virtual Output,
`app-monitor` for Virtual Monitor, `relay-render` for Relay Sink, and
`relay-capture` for Relay Microphone. The ACX bridge gives the app pair and
relay pair independent bounded PCM cables; live endpoint enumeration and
verifier gates are still required before the package can become installable.

The --audit-toolchain command is the explicit ACX gate. It checks
WDKContentRoot, the versioned KM CRT headers, acx.h, the target-architecture
`acxstub.lib`, and the compiler/LLVM executables before a driver build is
attempted. A nonzero result means the bootstrap driver remains fail-closed;
it is not evidence that an endpoint build succeeded.

After a passing audit, the opt-in binding compilation is:

    cargo check -p qpwgraph-audio --features acx

That command proves that the selected eWDK ACX headers and the feature-gated
device/circuit/stream bridge can be compiled. It does not prove that the
driver loads, enumerates an endpoint, or passes shared-mode, verifier, HLK, or
signing validation; the package remains fail-closed until those gates pass.

`install.ps1` and `uninstall.ps1` use PnPUtil for the privileged operation but
keep the lifecycle fail-closed. A release install requires the built
`qpwgraph-audio.inf`, `.cat`, `.sys`, a `ready` manifest with a driver version, and the
`qpwgraph-audio-smoke` probe. It waits for and verifies all four provider-owned
endpoint roles, and removes the exact published `oemNN.inf` automatically if
that verification fails. `-SkipEndpointVerification` is available only for an
explicitly managed test operation. `-AllowTestSigned` additionally requires
Windows test-signing mode. The uninstaller requires the exact published
`oemNN.inf`, verifies that the roles disappear, and supports `-WhatIf`; it
never searches for or removes an unrelated driver package.

Example release lifecycle commands:

```powershell
.\install.ps1 -SmokeProbe C:/path/to/qpwgraph-audio-smoke.exe
.\uninstall.ps1 -PublishedInf oem42.inf -SmokeProbe C:/path/to/qpwgraph-audio-smoke.exe
```

After a signed package is installed on a Windows test machine, run the smoke
probe with `--render-name "QPWGraph Virtual Output"` or the exact
`--render-id` printed by `--list`. `--round-trip` additionally opens
`QPWGraph Virtual Monitor`, writes a deterministic tone to the render stream,
and requires non-silent captured PCM. Without `--round-trip`, the probe only
exercises shared-mode open/start/stop/reset. It exits with code 2 when the
requested endpoint is absent.
