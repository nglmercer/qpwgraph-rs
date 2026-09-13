# QPWGraph Windows virtual-audio package

This is a nested Rust workspace so normal application builds never acquire a
WDK dependency. Use an eWDK/WDK developer prompt with KMDF 1.31 or newer and a
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

The Rust tool audit and the release audit honor `LIBCLANG_PATH` and also
search side-by-side `LLVM*\bin` installations, so an unsupported newer LLVM
on the default PATH does not hide a usable released toolchain.

`--build-package` builds the ACX-enabled release driver, stamps the INF,
generates `qpwgraph-audio.cat`, and stages the installable file set under
`drivers/windows-audio/target/qpwgraph-audio-package`. The source manifest in
this directory remains `bootstrap-fail-closed`; the generated manifest is
marked `ready` only after the real `.sys` and catalog have been produced.

`build-release-driver.ps1` is stricter than the development staging command:
it refuses to create a production candidate while any project-authored C/C++
runtime source remains under `driver/src`. The Rust ACX runtime still requires
the WDK and live validation gates before it can become a release candidate.

## Development test signing

The staged package is unsigned. On a disposable test VM, run the bundled
helper from the staged package in a WDK/eWDK developer prompt:

```powershell
Push-Location drivers/windows-audio/target/qpwgraph-audio-package
.\sign-test.ps1 -CreateCertificate
Pop-Location
```

The helper signs `qpwgraph_audio.sys`, regenerates the catalog so its hashes
match the signed driver, signs `qpwgraph-audio.cat`, and verifies the exact
catalog/INF/SYS set that will be installed. It does not change boot settings,
install the package, or import the certificate unless `-ImportCertificate` is
passed. Import the
printed `.cer` into `LocalMachine\Root` and `LocalMachine\TrustedPublisher`
on the test machine from an elevated PowerShell prompt. An existing code
signing certificate can be selected with
`-CertificateThumbprint <thumbprint>` instead.

When `-CreateCertificate` is used without `-ImportCertificate`, the helper
temporarily trusts only the generated public certificate in the current
user's `Root` store so SignTool can verify the exact package. It removes that
temporary trust before returning; it does not make the certificate trusted for
installation or change any machine-wide store.

For a guided elevated flow, use the staged `run-validation.cmd` launcher. It
uses `ExecutionPolicy Bypass`, requests a Windows UAC elevation when needed,
and writes the complete result to `validation-last.log`, so the validation
output can be inspected without copying it from a separate administrator
console. Each phase is explicit; only phases passed `-Reboot` restart the
machine:

```text
Set-Location drivers/windows-audio/target/qpwgraph-audio-package
.\run-validation.cmd -Phase Prepare
.\run-validation.cmd -Phase EnableTestMode -Reboot
# After Windows restarts in Test Mode:
.\run-validation.cmd -Phase Install
.\run-validation.cmd -Phase Smoke
# Replace oem42.inf with the exact name printed by Install:
.\run-validation.cmd -Phase Uninstall -PublishedInf oem42.inf
.\run-validation.cmd -Phase DisableTestMode -Reboot
```

The workflow cannot change the UEFI Secure Boot setting from Windows. The
`EnableTestMode` phase checks Secure Boot before touching BCD and reports the
exact `shutdown.exe /r /fw /t 0` handoff when firmware configuration is
required. Disable Secure Boot manually on the disposable test machine, boot
back into Windows, and rerun that phase. Normal `TESTSIGNING` mode is not
compatible with Secure Boot; a Secure-Boot-on validation requires a Microsoft
preproduction/production-signed package instead.

The launcher requires Administrator elevation, builds the smoke probe during
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
& $smoke --round-trip --duration-ms 5000
& $smoke --relay-round-trip --duration-ms 5000
& $smoke --verify-cables --duration-ms 5000
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
build and a test-signed Windows validation pass prove that path. A newly
staged candidate still needs its own disposable-machine install evidence before
it can be treated as live validated. This default fail-closed state prevents an
unvalidated development binary from being confused with a successful release
driver.
The installer checks `manifest.json` and refuses packages whose
`implementation_status` is not `ready`.

The install metadata is `package/qpwgraph-audio.inx`; the copy beside the
driver source is retained as a template for driver-local builds.

The package must never set a Windows default audio device. Test signing is for
development machines only; public packages require Microsoft signing and
Secure Boot validation.

The four endpoint roles are published as provider-owned custom endpoint
properties in the INF `HKR,EP\0` sections, so
`IMMDevice::OpenPropertyStore` can read them: `app-render` for Virtual
Output, `app-monitor` for Virtual Monitor, `relay-render` for Relay Sink, and
`relay-capture` for Relay Microphone. The matching typed `AddProperty`
sections remain on each interface for device-property consumers. The ACX
runtime gives the app pair and relay pair independent bounded PCM cables. The
recorded Windows 10 test-signed baseline verified all four roles and the app
cable's non-silent round trip against the then-installed development package;
it does not validate the newly staged candidate. Driver Verifier, HLK,
release-signing, Secure Boot, and ordinary-client relay tests remain separate
release gates.

The `--audit-toolchain` command is the explicit ACX gate. It checks
WDKContentRoot, the versioned KM CRT headers, acx.h, the target-architecture
`acxstub.lib`, and the compiler/LLVM executables before a driver build is
attempted. A nonzero result means the bootstrap driver remains fail-closed;
it is not evidence that an endpoint build succeeded.

`release-audit.ps1` is a read-only release-gate report for a build or test
machine. It checks the staged package shape and signatures, WDK/compiler/HLK
availability, verifier and Secure Boot state, test-signing state, installed
provider devices, and ordinary-client availability. It also lists the manual
HLK, Microsoft-signing, lifecycle, and ordinary-client acceptance rows that
cannot be proven by inspection. It never installs, signs, enables, disables,
restarts, or removes anything. By default it reports all findings and exits
zero so it can be collected on an incomplete machine; `-Strict` exits nonzero
when any row is blocked or unknown, and `-Json` emits a machine-readable report:

```powershell
Push-Location drivers/windows-audio/package
.\release-audit.ps1
.\release-audit.ps1 -Json > release-audit.json
.\release-audit.ps1 -Strict
Pop-Location
```

After the live acceptance work, a reviewer may provide a separately retained
evidence record with `-EvidencePath`. Only named manual rows are read from the
record; automatic machine checks still run normally. The record is deliberately
small and auditable:

```json
{
  "schema": 1,
  "gates": {
    "HLK audio tests complete": {
      "status": "pass",
      "evidence": "HLK result bundle: \\share\\qpwgraph\\hlk-2026-09-06.zip"
    },
    "Chrome/VLC ordinary relay acceptance": {
      "status": "pass",
      "evidence": "client-matrix log: chrome-vlc-relay-2026-09-06.txt"
    }
  }
}
```

Run the report with that record using
`.\release-audit.ps1 -EvidencePath .\acceptance-evidence.json -Strict -Json`.
The script validates the status/evidence shape but does not claim to validate
the truth of an externally supplied result; retain the referenced HLK, client,
power, and signing artifacts with the release record.

When a staged package exists, the script audits it automatically; otherwise it
audits the source package and reports the expected missing build artifacts.

`lifecycle-validation.ps1` supplies the remaining power and PnP lifecycle
procedure without broad device searches. It is plan-only unless `-Execute` is
provided, targets only `ROOT\DEVGEN\QPWGRAPH_AUDIO`, and runs role, cable, and
endpoint-absence smoke checks before and after each transition. Disable/enable
can be exercised with:

```powershell
.\lifecycle-validation.ps1 -Phase DisableEnable -Execute -Verbose
```

Suspend/resume is separately guarded by `-AllowSuspend` because it changes the
machine power state:

```powershell
.\lifecycle-validation.ps1 -Phase SleepResume -Execute -AllowSuspend -Verbose
```

`-Phase All` runs both procedures. A failed disable/enable pass attempts to
re-enable the exact devnode in a `finally` block. The script never changes boot
configuration, installs or removes a package, or disables an unrelated device;
preserve its output as the lifecycle acceptance record.

Audio service recovery is a separate explicit phase and restarts only
`Audiosrv`:

```powershell
.\lifecycle-validation.ps1 -Phase AudioService -Execute -AllowAudioServiceRestart -Verbose
```

The script verifies both cables before and after the restart. Reboot, client
crash, uninstall/upgrade, and physical endpoint churn remain separate live
rows because they require a disposable image and their own evidence.

The release-gate helpers are explicit about machine state:

```powershell
# Read-only package and machine evidence:
.\prepare-hlk.ps1 -OutputPath .\hlk-preparation.json
.\secure-boot-audit.ps1 -OutputPath .\secure-boot-audit.json
.\collect-verifier-evidence.ps1 -OutputPath .\verifier-evidence.json

# State-mutating Verifier controls; both require an explicit confirmation:
.\enable-verifier.ps1 -ConfirmEnable
.\run-driver-stress.ps1 -Execute -Cycles 100 -PackageRoot . -EvidencePath .\driver-stress.json
.\disable-verifier.ps1 -ConfirmReset
```

The Verifier scripts never run from a normal build. Enabling/resetting
Verifier is machine-wide and normally requires a reboot; pass the separate
`-Reboot -AllowReboot` switches only on a disposable test image. The stress
runner opens/stops the exact four endpoint roles through the smoke probe and
does not claim that client-crash, HLK, Microsoft-signing, or Secure Boot rows
passed. Its structured evidence requires 100 or more completed app-cable,
relay-cable, and two-cable-isolation cycles before the full-release validator
will accept it. Preserve its output with the read-only evidence JSON.

Retain the human-reviewed lifecycle/client record as
`acceptance-evidence.json` in the same evidence directory. It uses the
`release-audit.ps1 -EvidencePath` schema, and the full-release validator
requires every listed external gate to have `status: "pass"` with a non-empty
evidence reference.

For a production-signing boundary, build a candidate and prepare an external
dashboard submission without credentials:

```powershell
.\build-release-driver.ps1
.\prepare-dashboard-submission.ps1 -OutputDirectory C:\evidence\qpwgraph-submission
.\verify-returned-driver.ps1 `
  -PackageRoot C:\evidence\returned-driver `
  -SubmissionManifest C:\evidence\qpwgraph-submission\submission-manifest.json
.\validate-release-evidence.ps1 `
  -EvidenceRoot C:\evidence\release-evidence `
  -PackageRoot C:\evidence\returned-driver `
  -OutputPath C:\evidence\release-evidence\evidence-validation.json
```

`verify-returned-driver.ps1` requires valid SYS/CAT signatures, Microsoft
publisher identity by default, catalog membership, and all four semantic role
declarations. Supplying the submission manifest additionally requires exact
INF/manifest identity and records the allowed SYS/CAT signing or dashboard
transformation relationship. Test-signed packages must be verified separately
with `-AllowNonMicrosoft` for development only and must not be used as
production evidence.

After a passing audit, the opt-in binding compilation is:

    Push-Location drivers/windows-audio
    cargo check -p qpwgraph-audio --features acx --locked
    Pop-Location

That command proves that the selected eWDK ACX headers and the feature-gated
device/circuit/stream runtime can be compiled. The test-signed Windows pass
also proves that the recorded installed development package loaded, enumerated
all four roles, and passed the basic shared-mode round trip. A newly staged
candidate still needs its own install/run record. The package remains
development-only until Verifier, HLK, release-signing, Secure Boot, and
ordinary-client gates pass.

`install.ps1` creates the development-only `ROOT\DEVGEN\QPWGRAPH_AUDIO` devnode with
WDK `devgen.exe`, then uses PnPUtil for package installation and removal while
keeping the lifecycle fail-closed. A release install requires the built
`qpwgraph-audio.inf`, `.cat`, `.sys`, a `ready` manifest with a driver version, and the
`qpwgraph-audio-smoke` probe. It waits for and verifies all four provider-owned
endpoint roles, and removes the exact published `oemNN.inf` automatically if
that verification fails. `-SkipEndpointVerification` is available only for an
explicitly managed test operation. `-AllowTestSigned` additionally requires
Windows test-signing mode and verifies the staged catalog signature plus its
INF/SYS membership before invoking PnPUtil. The uninstaller requires the exact published
`oemNN.inf`, verifies that the roles disappear, and supports `-WhatIf`; it
never searches for or removes an unrelated driver package.

Example release lifecycle commands:

```powershell
.\install.ps1 -SmokeProbe C:/path/to/qpwgraph-audio-smoke.exe
.\uninstall.ps1 -PublishedInf oem42.inf -SmokeProbe C:/path/to/qpwgraph-audio-smoke.exe
```

After a signed package is installed on a Windows test machine, run the smoke
probe with `--render-name "QPWGraph Virtual Output"` or the exact
`--render-id` printed by `--list`. `--round-trip` selects the provider-owned
`app-render` and `app-monitor` roles, while `--relay-round-trip` selects
`relay-render` and `relay-capture`; both write a deterministic tone to the
render stream and require non-silent captured PCM. Without either round-trip
option, the probe only exercises shared-mode open/start/stop/reset. It exits
with code 2 when the requested endpoint is absent.

`--verify-cables` drives each render endpoint in turn while reading both
capture endpoints. It requires audio on the matching cable, actual silent
packets on the other cable, and silence after rendering stops (one second
to drain queued audio followed by a half-second measurement). Run this on a
quiet test machine with no other clients rendering to either virtual cable.
The app cable uses a 1 kHz tone and the relay cable uses 2 kHz; the probe
requires the expected tone and reports both amplitudes on both captures,
so a failure can distinguish the active test signal from previous-cable audio.
Non-finite PCM fails the probe. Tone analysis uses only the first channel;
peak and silence checks cover all channels.
This detects cross-talk and stale audio across stream restarts; it does not
replace relay peer-disconnect or ordinary-client acceptance. Run live capture
probes outside restricted process sandboxes: the restricted execution context
can make WASAPI capture initialization fail with `0x80070057` even when the
same binary succeeds in the normal user context.

`powershell -File tests/install-binding.ps1` separately exercises the install
verification gate without mutating devices. The gate requires the devnode's
bound INF to equal the exact published package and its problem code to be
zero before accepting endpoint roles. Existing endpoints from an older
package cannot prove an upgrade succeeded.

The EOS boundary regression now lives in the no-std Rust driver core. Run
`cargo test -p qpwgraph-audio-core render_eos --locked` from the nested
workspace; the Rust ACX runtime consumes the result through a small
FFI-shaped transport boundary and honors the final byte length while
suppressing later circular-buffer data as required by the ACX render packet
contract.

## Client-visible timing and owned-client crash checks

Run `qpwgraph-audio-smoke --verify-timing --duration-ms 2000` after installing
the development driver. It selects all four provider-owned roles and runs each
clock initially, after Stop/Start, and after Reset/Start. It checks monotonic
position, declared-frequency agreement with correlated QPC timestamps,
250 ms of unchanged position after Stop, and zero position after Reset.

Device positions are converted using `IAudioClock::GetFrequency`, not assumed
to be sample frames. The 50 ms maximum accumulated clock error is an explicit
smoke-test bound, **not an HLK limit**. A polling gap over 250 ms, zero frames,
insufficient observations, or an inaccurate `S_FALSE` reading fails the probe.
Each running phase lasts at least two seconds; short requested durations do
not weaken the measurement. This is shared-mode client-visible evidence, not
long-run counter-wrap or certification evidence. For direct kernel timing,
use `cargo run -p qpwgraph-audio-ks-probe --locked -- --verify-timing`; it
correlates `KSPROPERTY_RTAUDIO_PRESENTATION_POSITION` block positions with
QPC timestamps on all four endpoints and verifies pause/STOP behavior.
Direct one- and two-packet KS EOS coverage is available from the workspace
probe: `cargo run -p qpwgraph-audio-ks-probe --locked -- --verify-eos`. The
same run checks that a non-EOS packet may carry an oversized ignored
`EosPacketLength` while EOS packets remain bounded and frame-aligned.
Use `--verify-formats` to verify that the four canonical endpoints reject a
valid but unsupported 44.1 kHz stereo PCM16 pin request.
Use `--verify-jacks` to query one `KSJACK_DESCRIPTION` for each bridge pin and
verify its channel map and connection/location metadata.
Use `--verify-lifecycle` to exercise 17 direct KS start/pause/resume/stop
cycles and reopen behavior on all four endpoints. The count deliberately
exceeds the driver's 8-slot stream registry so slot leaks become observable;
Driver Verifier is still required for a release leak gate.

For abrupt process death, use the default read-only plan first, then opt in:

```powershell
./run-client-crash.ps1
./run-client-crash.ps1 -Execute -Cycles 3 -SmokeProbe C:/path/to/qpwgraph-audio-smoke.exe -EvidencePath C:/path/to/new-client-crash.json
```

The crash script waits for its own newly launched helper to report active PCM
and the matching PID, kills that process only, then verifies both cables without
automatic retries. It preserves hashes, readiness, child exit status, and every
recovery result. Existing evidence files are rejected instead of overwritten.
It does not change Test Mode, services, devices, defaults, or existing apps.
Without `-Independent`, both render and capture die together in each child.
For separate-client survivor coverage, use:

```powershell
./run-client-crash.ps1 -Independent -Execute -Cycles 1 -SmokeProbe C:/path/to/qpwgraph-audio-smoke.exe -EvidencePath C:/path/to/independent-client-crash.json
```

This starts one owned render and one owned capture child per cable, terminates
each flow in turn, verifies the peer remains alive for 750 ms, and then checks
both cables after cleanup. qpwgraph backend crashes and Verifier still need
separate tests. Run on a quiet machine with no other virtual-cable producers.
