# QPWGraph Windows virtual-audio package

This is a nested Rust workspace so normal application builds never acquire a
WDK dependency. Use an eWDK/WDK developer prompt with KMDF 1.33 and a
released LLVM 17--21 toolchain available; LLVM 22 currently breaks bindgen's
WDK layout generation:

```powershell
Push-Location drivers/windows-audio
$env:LIBCLANG_PATH = 'C:\LLVM21\bin' # adjust to your LLVM 17--21 installation
$env:Path = "$env:LIBCLANG_PATH;$env:Path"
cargo test -p qpwgraph-audio-core --locked
cargo run -p qpwgraph-audio-xtask --locked -- --validate-package
cargo run -p qpwgraph-audio-xtask --locked -- --audit-toolchain
cargo run -p qpwgraph-audio-xtask --locked -- --build-package
cargo run -p qpwgraph-audio-smoke -- --list
Pop-Location
```

`--build-package` builds the ACX-enabled release driver, stamps the INF,
generates `qpwgraph-audio.cat`, and stages the installable file set under
`drivers/windows-audio/target/qpwgraph-audio-package`. The source manifest in
this directory remains `bootstrap-fail-closed`; the generated manifest is
marked `ready` only after the real `.sys` and catalog have been produced.

## Development test signing

The staged package is unsigned. On a disposable test VM, run the bundled
helper from the staged package in a WDK/eWDK developer prompt:

```powershell
Push-Location drivers/windows-audio/target/qpwgraph-audio-package
.\sign-test.ps1 -CreateCertificate
Pop-Location
```

The helper signs `qpwgraph_audio.sys`, regenerates the catalog so its hashes
match the signed driver, and signs `qpwgraph-audio.cat`. It does not change
boot settings, install the package, or import the certificate. Import the
printed `.cer` into `LocalMachine\Root` and `LocalMachine\TrustedPublisher`
on the test machine from an elevated PowerShell prompt. An existing code
signing certificate can be selected with
`-CertificateThumbprint <thumbprint>` instead.

For a guided elevated flow, use the staged `test-validation.ps1` runner. Each
phase is explicit; only phases passed `-Reboot` restart the machine:

```powershell
Set-Location drivers/windows-audio/target/qpwgraph-audio-package
.\test-validation.ps1 -Phase Prepare
.\test-validation.ps1 -Phase EnableTestMode -Reboot
# After Windows restarts in Test Mode:
.\test-validation.ps1 -Phase Install
.\test-validation.ps1 -Phase Smoke
# Replace oem42.inf with the exact name printed by Install:
.\test-validation.ps1 -Phase Uninstall -PublishedInf oem42.inf
.\test-validation.ps1 -Phase DisableTestMode -Reboot
```

The runner requires Administrator elevation, builds the smoke probe during
`Prepare`, imports only the public test certificate, creates the development
root devnode through the WDK `devgen.exe` tool, performs role and round-trip
verification, and never enables `-SkipEndpointVerification`.

Build the smoke probe before installation:

```powershell
cargo build --manifest-path drivers/windows-audio/tests/smoke/Cargo.toml --locked
$smoke = (Resolve-Path drivers/windows-audio/target/debug/qpwgraph-audio-smoke.exe).Path
```

In an elevated Command Prompt, enable test signing and reboot the disposable
test machine:

```text
bcdedit /set testsigning on
shutdown /r /t 0
```

After reboot, verify the Test Mode watermark and install with endpoint
verification enabled:

```powershell
Push-Location drivers/windows-audio/target/qpwgraph-audio-package
.\install.ps1 -AllowTestSigned -SmokeProbe $smoke -Verbose
Pop-Location
```

Record the exact `oemNN.inf` printed by the installer. The smoke probe then
provides the live gates:

```powershell
& $smoke --verify-roles
& $smoke --list
& $smoke --render-name 'QPWGraph Virtual Output' --capture-name 'QPWGraph Virtual Monitor'
& $smoke --round-trip --duration-ms 5000
```

Do not use `-SkipEndpointVerification` for the acceptance pass. If the
package or endpoint verification fails, the installer rolls back the exact
published package. For a successful run, uninstall with the recorded package
name and verify disappearance:

```powershell
Push-Location drivers/windows-audio/target/qpwgraph-audio-package
.\uninstall.ps1 -PublishedInf oemNN.inf -SmokeProbe $smoke -Verbose
Pop-Location
```

Only after uninstalling should test signing be disabled and the machine
rebooted with `bcdedit /set testsigning off`. Driver Verifier, HLK, Secure
Boot, upgrade, and client-application tests remain separate release gates.

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

The `--audit-toolchain` command is the explicit ACX gate. It checks
WDKContentRoot, the versioned KM CRT headers, acx.h, the target-architecture
`acxstub.lib`, and the compiler/LLVM executables before a driver build is
attempted. A nonzero result means the bootstrap driver remains fail-closed;
it is not evidence that an endpoint build succeeded.

After a passing audit, the opt-in binding compilation is:

    Push-Location drivers/windows-audio
    cargo check -p qpwgraph-audio --features acx --locked
    Pop-Location

That command proves that the selected eWDK ACX headers and the feature-gated
device/circuit/stream bridge can be compiled. It does not prove that the
driver loads, enumerates an endpoint, or passes shared-mode, verifier, HLK, or
signing validation; the package remains fail-closed until those gates pass.

`install.ps1` creates the development-only `ROOT\DEVGEN\QPWGRAPH_AUDIO` devnode with
WDK `devgen.exe`, then uses PnPUtil for package installation and removal while
keeping the lifecycle fail-closed. A release install requires the built
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
