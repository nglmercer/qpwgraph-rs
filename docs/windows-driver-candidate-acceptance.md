# Rust Windows driver candidate acceptance

Development validation on September 12–13, 2026. This supersedes the older
installed-package identity in `windows-driver-acceptance-baseline.md`, but
does not transfer that baseline's client/policy acceptance to this candidate.

Core acceptance uses generic WASAPI and deterministic project-owned helpers.
Brand-specific application checks are optional compatibility sampling and do
not require extra third-party applications to be installed.

## Installed package

- Driver source commit: `25ddbe6` (guarded idle-cable clearing during device release).
- Windows 10 Pro; exact devnode `ROOT\DEVGEN\QPWGRAPH_AUDIO`.
- Published INF: `oem24.inf`; INF DriverVer `09/13/2026,11.8.28.7`.
- Service `qpwgraph_audio` running, devnode problem code 0.
- Existing development signer: `CN=QPWGraph Audio Test`, certificate
  `30D29DBE073E11B6308872DA7170B3371BE6C037`.
- SYS SHA-256 (signed staged package and installed file agree):
  `099F48379B892913BA6F585E253B07EA7DC7D9A0E48183BA579088A49D3259C2`.
- INF SHA-256:
  `2A6CFF5CB0EB9E8CAA938FC8F6763F182D13628CE00A07C5E2E712C9DA918D7D`.
- CAT SHA-256:
  `5149E53234FC0E9C200DAC946E0D455D9C4FEFCDF2D3F79F24F5738C87CE77BB`.

Signing, catalog membership verification, installation, active binding, and
four-role enumeration succeeded. Local transcripts (ignored build outputs):

- `drivers/windows-audio/target/candidate-20da4d7-install.log`
- `drivers/windows-audio/target/candidate-20da4d7-status.log`

Test-signing was verified **True**, Secure Boot **False**. No boot settings
or system default audio devices were changed. Keep Test Mode enabled on this
PC; full parity and readiness to disable Test Mode are **not complete**.

A read-only release audit against the refreshed staged package reported 15
core/toolchain passes, 2 direct environment blocks, and 16 unproven core
gates. It now discovers
the installed supported LLVM 21 at `C:\LLVM21\bin` even though the default
shell also exposes LLVM 22. The remaining direct blocks are missing HLK
Studio and intentionally disabled Secure Boot; the audit made no system
changes.

The sections through the owned-client crash checks below retain the original
`oem21.inf`/`20da4d7` evidence. They are historical, intentionally preserved
for auditability; the currently installed `oem24.inf` package and its hashes
are recorded above and its post-install checks are recorded in the direct KS
section below.

## Initial live smoke (historical oem21 package)

From repository root:

```powershell
& ./drivers/windows-audio/target/debug/qpwgraph-audio-smoke.exe --verify-cables --duration-ms 1500
```

Exit 0. All four semantic roles verified. App target peak 0.208038, relay
target peak 0.250031; the opposite cable stayed at peak 0 in both tests.
Stopped-render capture delivered 24,000 silent frames per check.

## Interrupted stress run — retained failure, not acceptance

`drivers/windows-audio/target/candidate-20da4d7-stress.json` records:

- Start: `2026-09-12T16:11:00.9256516Z`.
- Failure: `2026-09-12T17:33:14.8246111Z`.
- 100/100 app round trips passed, 100/100 relay round trips passed.
- 36/100 isolation cycles completed; cycle 37 failed after its first tone
  check passed. Stopped-render measurement received 0 target frames, with
  peak 0 on both captures. The old probe did not count other-cable silence
  frames separately.
- The original script incorrectly labeled the partial row `not-run`;
  `completed=false`, the cycle counter, and failure string are authoritative.

Read-only System event-log correlation:

- Kernel-Power event 42: sleep requested by Button or Lid.
- Power-Troubleshooter event 1: sleep at
  `2026-09-12T16:15:13.745718700Z`, wake at
  `2026-09-12T17:33:15.134386200Z`.
- Kernel-General event 1: clock resynchronized from
  `2026-09-12T16:15:15.767045800Z` to `2026-09-12T17:33:12.500000000Z`.

The fixed-duration measurement window was interrupted by host suspension.
This explains why the run is unsuitable as uninterrupted stress evidence;
it does not establish clean active-stream recovery from sleep or rule out
a driver recovery defect. Do not erase or relabel this failure after a rerun.

## Validation tooling corrections

- Silence now requires actual packets from **both** capture endpoints;
  regression tests reject missing packets, signal, NaN, and infinity.
- A failed silence check reports elapsed time and the largest polling gap
  to distinguish an interrupted observation from ordinary no-packet output.
  Failures are not automatically retried or converted to passes.
- Stress rows retain explicit failure status, exact failed cycle, UTC row
  timestamps, per-probe failure timestamps, and the smoke executable hash.
- Windows 10 PnpDevice commands implemented as functions are recognized by
  the status and lifecycle scripts. Mock regression tests never touch devices.
- The application/relay integration test now declares its direct effects
  dependency, fixing the observed unresolved-crate build error.

## Uninterrupted stress recheck — passed

The rebuilt probe used the stricter two-capture silence check. Command:

```powershell
& ./drivers/windows-audio/package/run-driver-stress.ps1 -Execute -Cycles 100 -DurationMilliseconds 250 -PackageRoot ./drivers/windows-audio/target/qpwgraph-audio-package -EvidencePath ./drivers/windows-audio/target/candidate-20da4d7-stress-recheck.json
```

Exit 0; `completed=true`, all three cable rows explicitly `passed`:

- UTC start `2026-09-13T09:27:59.8276433Z`.
- UTC completion `2026-09-13T09:37:49.2177875Z`.
- 100/100 app round trips, 100/100 relay round trips, 100/100 two-cable checks.
- Probe SHA-256 `969AEBC93D5DCCCA1B2F02205477CE178675F3CA96B0F33B52E33A08430A7E1F`.
- Package hashes still match the installed candidate above.
- No Kernel-Power or Power-Troubleshooter events were returned from the
  System log for this interval.

This is ordinary driver stress, **not Driver Verifier evidence**. AudioSrv
restart and device toggle were intentionally separate from this matrix.

## Current-source ordinary stress matrix — passed September 13

The installed `25ddbe6` source candidate was exercised again with the current
smoke helper and the stricter two-capture silence check:

```powershell
& ./drivers/windows-audio/package/run-driver-stress.ps1 -SmokeProbe ./drivers/windows-audio/target/debug/qpwgraph-audio-smoke.exe -PackageRoot ./drivers/windows-audio/target/qpwgraph-audio-package -EvidencePath ./drivers/windows-audio/target/candidate-current-source-r8-ordinary-stress-20260913.json -Cycles 100 -DurationMilliseconds 250 -Execute -Verbose
```

Exit 0; `completed=true`, with 100/100 passed cycles in each row:

- app cable: `2026-09-13T19:33:49.501277Z` to
  `2026-09-13T19:34:22.1658795Z`;
- relay cable: `2026-09-13T19:34:22.166695Z` to
  `2026-09-13T19:34:54.6679885Z`;
- two-cable isolation: `2026-09-13T19:34:54.668131Z` to
  `2026-09-13T19:43:31.264974Z`.

The smoke helper SHA-256 is
`50CA81C05FCB785DD33BE7D8B0C8B18C3B0467D97AEB46F141115D55D5F2C237`; the
package hashes match the installed candidate above. The matrix explicitly
records AudioSrv restart and device toggle as `not-run`; those are separate
lifecycle rows. This is ordinary driver stress, **not Driver Verifier
evidence**.

## Driver Verifier observation — no driver configured

A read-only collection was saved at
`drivers/windows-audio/target/candidate-current-source-r8-verifier-observation-20260913.json`.
`verifier /querysettings` was available and reported `Verifier Flags:
0x00000000`; `verifier /query` reported that no drivers are currently
verified. The same record reports Secure Boot disabled and a non-elevated boot
configuration query that could not be read. No Verifier setting, boot setting,
service, device, or reboot was changed by this collection.

This is an observation only. It does not prove Verifier cleanliness; the
driver-scoped Verifier run and its recovery/event evidence remain open.

## Device disable/enable — passed

Executed elevated:

```powershell
& ./drivers/windows-audio/package/lifecycle-validation.ps1 -Phase DisableEnable -Execute -PackageRoot ./drivers/windows-audio/target/qpwgraph-audio-package -Verbose
```

Exit 0; completed at `2026-09-13T09:38:22.4650395Z`. Transcript:
`drivers/windows-audio/target/candidate-20da4d7-lifecycle-DisableEnable.log`.

Pre-toggle roles/cables passed. Only `ROOT\DEVGEN\QPWGRAPH_AUDIO` was
disabled. Endpoint disappearance and reappearance each needed one normal
poll retry while PnP notifications settled; those transient failures remain
visible in the transcript. All four roles and both cables passed afterward,
including 24,000 silent frames on each capture in each stopped-render check.
Binding remained `oem21.inf`, problem code 0, with the same installed SYS hash.
Read-only boot verification still reported `testsigning Yes`.

This covers an idle-device lifecycle transition with fresh clients before and
afterward, not client survival during a device removal or a repeated toggle soak.

## Windows Audio service restart — passed

Executed elevated with explicit service-restart opt-in:

```powershell
& ./drivers/windows-audio/package/lifecycle-validation.ps1 -Phase AudioService -Execute -AllowAudioServiceRestart -PackageRoot ./drivers/windows-audio/target/qpwgraph-audio-package -Verbose
```

Exit 0; completed at `2026-09-13T09:38:49.4807770Z`. Transcript:
`drivers/windows-audio/target/candidate-20da4d7-lifecycle-AudioService.log`.
All four roles and both cable/isolation/silence checks passed before and after
restarting `Audiosrv`. The installed binding/hash stayed unchanged, problem
code remained 0, and boot verification still reported `testsigning Yes`.
This verifies fresh-client recovery, not survival of a pre-existing app stream.

## Automatic Win32 helper routing — passed

After fixing the test's missing direct effects dependency, built with
`cargo test -p windows-audio-test-tone --features relay-tests --test relay_microphone --no-run --locked`.
Both following tests actually ran with their named opt-in variable set to `1`,
`--exact --nocapture --test-threads=1`, and exited 0:

| Test in `relay_microphone::live` | Opt-in environment variable | Observed result |
| --- | --- | --- |
| `experimental_application_route_restores_on_driver_shutdown` | `PW_GRAPH_TEST_WINDOWS_AUTO_APP_ROUTE_DROP` | All three roles moved to AppRender; replacement PID rebound; 1 kHz amplitude 0.2154; dropping backend restored original role values. |
| `experimental_application_route_rebinds_default_helper_and_restores` | `PW_GRAPH_TEST_WINDOWS_AUTO_APP_ROUTE` | All three roles moved to AppRender; replacement PID rebound; 1 kHz amplitude 0.2162; removing rule restored original role values. |

Initial snapshots were `None` for Console, Multimedia, and Communications.
The tests mutated only their disposable helper's policy and stopped their
children afterward. Test flags were scoped to the invoking shell, not enabled
in the user's application configuration. Local logs:

- `drivers/windows-audio/target/candidate-20da4d7-policy-shutdown.log`
- `drivers/windows-audio/target/candidate-20da4d7-policy-rule-removal.log`

The earlier standalone `E_INVALIDARG` did not recur with a real audio-producing
helper. These live results supersede the claim that automatic switching is
universally blocked on this PC; they do not explain every unsupported-process
case or establish MSIX, user-override, or full application restart acceptance.

September 17 addendum: the three gaps named above have since closed on the
`25ddbe6`/`oem24.inf` candidate — MSIX auto-move
(`packaged_msix_application_route_rebinds_and_restores`, post-restart 1 kHz
at 0.1325 amplitude), manual-override preservation
(`PW_GRAPH_TEST_WINDOWS_APP_ROUTE_OVERRIDE=1`), and application restart
re-application (replacement-PID flow inside the auto-route tests). Only
unsupported-process explanation and unsupported-build fallback remain open.

Focused smoke clippy passed with warnings denied. Application integration-test
clippy passed with `--no-deps`; the broader invocation still reports the
existing `clippy::too_many_arguments` warning in backend `routing.rs::walk`.

Policy unit tests (`cargo test -p pw-graph-backend --lib audio_policy_config::tests --locked -- --test-threads=1`):
10 passed, 1 opt-in standalone live probe ignored. Coverage includes build/IID
gating, interface layout, endpoint-ID round trips, display-name-only rejection,
manual-override ownership, and failure demotion. The ignored test is not counted
as live evidence; the two explicitly enabled helper tests above are.

## Relay Microphone reconnect — passed

`live::peer_audio_reaches_ordinary_relay_microphone_client` ran with
`PW_GRAPH_TEST_RELAY_MICROPHONE=1` and `PW_GRAPH_TEST_RELAY_MICROPHONE_CYCLES=3`.
Exit 0, 21.09 seconds. Initial 1 kHz amplitude 0.2334; reconnect amplitudes
0.2305, 0.2279, and 0.2310. Each disconnect measured 48,000 silent frames
(peak 0.000031); the driver stayed running across reconnects. This uses an
ordinary WASAPI microphone consumer and satisfies the core public capture
client contract; branded application checks are optional.

Transcript:
`drivers/windows-audio/target/candidate-20da4d7-peer_audio_reaches_ordinary_relay_microphone_client.log`.

## Initial effects probe — retained test-API failure

The next live probe failed before effect activation because it called the old
synchronous `create_effect_node` API. The backend correctly rejected this with
`Windows effects must be created asynchronously with begin_create_effect`.
The helper was stopped on failure. Preserve the original transcript:
`drivers/windows-audio/target/candidate-20da4d7-isolated_application_effect_applies_and_bypass_restores_audio.log`.

The test now queues `EffectCreateRequest`, polls the matching ticket until
`Ready`, rejects failure/cancellation, and cancels on a bounded timeout before
connecting ports. This is a test migration, not a relaxation of the backend's
asynchronous-creation requirement or its silence/tone acceptance thresholds.

## Effect output routing correction — passed

The asynchronous test first exposed a genuine backend rejection when connecting
an activated effect output to a physical playback endpoint. Retained failure:
`drivers/windows-audio/target/candidate-20da4d7-effects-async-recheck.log`.

`WindowsAudioDriver::connection_support` now recognizes an output registered
in the live effects routing table for playback destinations, as it already
did for effect and recorder destinations. An `Effect` node label alone does
not grant routing permission; ordinary non-isolated sessions remain blocked.

The rerun passed (exit 0, 4.30 seconds): gate enabled delivered 87,759 frames
with peak/tone amplitude 0; bypass delivered 88,200 frames, peak 0.2500 and
1 kHz amplitude 0.2407. Transcript:
`drivers/windows-audio/target/candidate-20da4d7-effects-routing-recheck.log`.

The new backend regression test also passed on this PC. It checks registered
effect-to-playback support, rejection of a fake unregistered effect, continued
rejection of a capture-only session to playback/effect, and loss of support
after effect removal. It queries synthetic graph ports without opening audio
clients; like neighboring startup tests it may skip on headless Windows without
Core Audio, so the explicit live probe remains necessary acceptance evidence.

## Local-output preservation and isolated helper restart — passed

Both tests ran explicitly, one at a time, after the effect routing fix:

- `live::ordinary_application_relay_preserves_local_output`, with
  `PW_GRAPH_TEST_RELAY_LOCAL_OUTPUT=1`: exit 0 in 4.23 seconds. Physical
  output peak remained 0.2500 before and during ordinary application relay.
- `live::isolated_application_route_rebinds_after_helper_restart`, with
  `PW_GRAPH_TEST_WINDOWS_APP_ROUTE_RESTART=1`: exit 0 in 4.60 seconds.
  The stable selector rebound from PID 10636 to PID 5180; observed 1 kHz
  amplitudes were 0.2343 before restart and 0.2336 afterward.

Local transcripts are respectively
`drivers/windows-audio/target/candidate-20da4d7-ordinary_application_relay_preserves_local_output.log`
and
`drivers/windows-audio/target/candidate-20da4d7-isolated_application_route_rebinds_after_helper_restart.log`.
These deterministic project-helper checks satisfy the core application-relay
contract; branded application checks are optional compatibility sampling.

## Recommended next validation batch

1. Run the explicit driver-scoped Verifier matrix with a recovery plan and
   retained crash/event evidence; ordinary 300-cycle stress does not substitute
   for it.
2. On an isolated test window, cover qpwgraph backend crash recovery,
   pre-existing active-stream sleep/resume, hibernate/reboot, and repeated
   install/uninstall/upgrade rows. Do not treat the accidental host sleep in
   the retained failure as sleep/resume acceptance.
3. Complete manual-override and unsupported-build fallback behavior, then HLK
   and Microsoft signing/release gates on the required environments. MSIX and
   branded application checks are optional compatibility work.

Keep the development PC in Test Mode throughout. Secure Boot validation belongs
on the separate release-test environment, not a boot change on this machine.

## Shared-mode clock validation — passed September 13

Added `--verify-timing` to the smoke probe. It runs all four endpoints initially,
after Stop/Start, and after Reset/Start. Each phase services audio continuously
for a 100 ms startup period and at least two seconds of measurement, followed
by 250 ms of stopped-position checks. Reset must report position zero.

The probe follows the documented [IAudioClock position/frequency contract](https://learn.microsoft.com/en-us/windows/win32/api/audioclient/nf-audioclient-iaudioclock-getposition).
Positions use the returned device frequency, not an assumed frame count; QPC
timestamps are already in 100 ns units. It rejects non-monotonic observations,
inaccurate readings, missing frames, and polling gaps exceeding 250 ms.
The 50 ms maximum accumulated rate-error bound is a smoke-test choice, not
an HLK requirement. Ten smoke unit tests and strict smoke clippy passed.

```powershell
& ./drivers/windows-audio/target/debug/qpwgraph-audio-smoke.exe --verify-timing --duration-ms 2000
```

Exit 0. Transcript `drivers/windows-audio/target/candidate-20da4d7-timing-initial.log`.
Probe SHA-256 at that run:
`FF7F702FDACC0CC94EBD5BC6889AF0F0317FA17B80BF5B523AA7983F11973831`.
All clock frequencies were 384,000 units/second. Maximum accumulated error
against correlated QPC, by phase:

| Role | Initial | Resumed | After reset |
| --- | ---: | ---: | ---: |
| app-render | 686 us | 481 us | 362 us |
| app-monitor | 10,683 us | 19,976 us | 10,273 us |
| relay-render | 738 us | 791 us | 1,036 us |
| relay-capture | 10,262 us | 20,130 us | 10,248 us |

All stopped positions stayed fixed and resets returned zero; largest observed
polling gap was 17 ms. These are shared-mode client clocks, which include the
Windows audio engine. They cannot establish raw ACX presentation accuracy,
single-packet timing behavior, or actual delivery of a kernel EOS flag.
The driver SYS was not rebuilt or replaced during these checks.

## Owned round-trip process crashes — passed September 13

Added `package/run-client-crash.ps1`. It defaults to plan-only mode. Executed:

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File drivers/windows-audio/package/run-client-crash.ps1 -Execute -Cycles 3 -EvidencePath drivers/windows-audio/target/candidate-20da4d7-client-crash.json
```

Exit 0; UTC interval approximately `2026-09-13T14:28:48Z` through
`2026-09-13T14:29:25Z`. All six rows passed. Owned app-cable PIDs were 6212,
3540, 10088; owned relay-cable PIDs were 11424, 11260, 13224. Each reported
active non-silent round-trip PCM before termination and exited with code -1.
Both cables then passed tone/isolation/stopped-silence checks, without retries.

The JSON retains readiness lines, output, PID/exit status, UTC times, and hashes.
Probe SHA-256 after adding the active-PCM handshake:
`CF7610682B966199129425688FF646698D75316BD9D486DCA6F1C4449CAAACDB`.
Installed SYS remained
`5FBD69AE4A4C0C19965958BB11EA1E25A111F9AD386F42E5EC2462B48F9F6EB4`.

Only processes created by this test were terminated. No boot, service, device,
default endpoint, or user application configuration was changed. The handshake
regressions reject wrong PIDs, zero frames, and unrelated output; WhatIf exits
before process creation. The script refuses to overwrite existing evidence.
Package staging includes it, and normal Windows CI checks its syntax/handshake
without executing live crashes.

These rows prove recovery after **both clients die together**. They do not
prove render-only/capture-only crashes with another client surviving, qpwgraph
backend crash recovery, sleep/resume, or Driver Verifier cleanliness.

The final rebuilt probe also passed all four clock checks again after the six
crashes (`candidate-20da4d7-timing-after-crashes.log`, exit 0). Maximum accumulated
clock error was 20,314 us, and maximum polling gap was 40 ms. The installed
devnode remained bound to `oem21.inf` with problem code 0; both `qpwgraph_audio`
and `Audiosrv` were running. Reusing the existing crash evidence path was
separately tested: it failed before spawning a child and preserved the JSON hash.

The WASAPI checks above do not establish delivery of an ACX EOS packet.
The subsequent direct KS probe supplies the bounded evidence below.

## Direct KS EOS and sequence coverage — September 13

Added `tests/ks-probe` to the driver workspace. It enumerates interfaces only
for `ROOT\DEVGEN\QPWGRAPH_AUDIO`, requires unique circuit names and verifies
host-pin direction before opening. Default `--inspect` does not create streams;
`--open-pins` exercises mapping and state transitions without RUN.

```powershell
cargo run --manifest-path drivers/windows-audio/Cargo.toml -p qpwgraph-audio-ks-probe --locked -- --verify-eos
```

The same probe source also provides a format-negotiation check:

```powershell
cargo run --manifest-path drivers/windows-audio/Cargo.toml -p qpwgraph-audio-ks-probe --locked -- --verify-formats
```

It rejected a valid 44.1 kHz stereo PCM16 request on all four owned endpoints
with the current driver, confirming that unsupported formats fail closed.

The endpoint jack metadata query also passed on all four circuits:

```powershell
cargo run --manifest-path drivers/windows-audio/Cargo.toml -p qpwgraph-audio-ks-probe --locked -- --verify-jacks
```

Each bridge pin returned one `KSJACK_DESCRIPTION` with stereo channel map 3,
ATAPI-internal connection, front/primary-box location, and integrated-device
port metadata.

The direct lifecycle mode also passed on all four endpoints:

```powershell
cargo run --manifest-path drivers/windows-audio/Cargo.toml -p qpwgraph-audio-ks-probe --locked -- --verify-lifecycle
```

Each endpoint completed 17 start/pause/resume/stop cycles—more than twice the
driver's 8-slot stream registry—without slot exhaustion. Packet counts did
not advance while paused, and a fresh reopen started with packet count zero.

The direct presentation-position timing mode also passed on all four endpoints:

```powershell
cargo run --manifest-path drivers/windows-audio/Cargo.toml -p qpwgraph-audio-ks-probe --locked -- --verify-timing
```

It queried the driver's `KSPROPERTY_RTAUDIO_PRESENTATION_POSITION` response
for 750 ms per endpoint, checked monotonic audio blocks and QPC timestamps,
and compared the block slope with the declared 48 kHz format. The run
observed 71–72 position samples and 73–74 packets per endpoint with a maximum
error of one audio block. Pause held the position constant and explicit STOP
passed. This closes the bounded direct timing check; long-run counter-wrap,
preroll, Driver Verifier, and HLK timing evidence remain open.

All 20 live EOS cases passed on current candidate `25ddbe6` (ten on each
cable). A separate check on each render endpoint also accepted packet 1 with
flags clear and `EosPacketLength = u32::MAX`, proving that the non-EOS length
field is ignored while the actual packet mapping remains bounded.
The two-packet cases use 1920-byte packets at 48 kHz stereo PCM16 and final
lengths 0, 4, 16, 960 and 1920 bytes. The one-notification cases request 1920
bytes and verify the corrected 4096-byte page-aligned mapping, then reuse that
single mapped packet across the first-to-final boundary with lengths 0, 4, 16,
2048 and 4096 bytes. Every case submits
`KSSTREAM_HEADER_OPTIONSF_ENDOFSTREAM` directly through SETWRITEPACKET.

The oracle permits only leading capture underflow silence. It then requires
the complete first packet, the exact final prefix, zeroes through the rest of
the final packet, no poisoned/replayed/reordered samples, at least ten later
silent capture packets, and explicit PAUSE/ACQUIRE/STOP cleanup. Oversized and
unaligned EOS, undefined flags, late packets, skipped packets, and writes after
EOS were rejected (specific rejection status codes are not asserted).

Polling gaps or a changed packet during inspection fail as inconclusive;
the probe does not silently retry lost observations. Six unit tests cover the
sample oracle, native request layout, and circuit identity. Unit tests and
strict all-target clippy passed; both are included in Windows CI without live
driver access. The core crate has 22 passing tests, including shared EOS
argument validation, late/skipped admission, and `u32::MAX -> 0` sequence
cases. A post-install WASAPI
`--verify-timing --duration-ms 2000` and `--verify-cables --duration-ms 1500`
also passed with this current driver.

Retained local logs: `drivers/windows-audio/target/candidate-current-source-r8-formats.log`,
`candidate-current-source-r8-eos.log`, `candidate-current-source-r8-lifecycle.log`,
`candidate-current-source-r8-jacks.log`, `candidate-current-source-r8-timing.log`, and
`candidate-current-source-r8-cables.log` in the same directory. The repeated direct
KS lifecycle output is retained in `drivers/windows-audio/target/candidate-current-source-r8-repeated-lifecycle.log`,
and direct KS timing output is retained in `drivers/windows-audio/target/candidate-current-source-r8-direct-timing.log`.
Current
probe executable SHA-256:
`82607A50F0A369F0981FB05BEB932738AD9AABC570CAE44B9D15BDF63E928874`.
The staged and installed current SYS SHA-256 is
`099F48379B892913BA6F585E253B07EA7DC7D9A0E48183BA579088A49D3259C2`.
The package was installed without a reboot; no boot configuration or Secure
Boot setting was changed.

This establishes direct EOS for the tested one- and two-packet configurations,
including live skipped/late rejection, and bounded direct presentation timing.
It does not establish a real 32-bit-counter wrap run, preroll/long-run wrap
behavior, or certification. The full EOS gate remains open.

## Independent client crash recovery — September 13

The crash harness now has an explicit independent-client mode. It starts
separate owned render and capture helpers, confirms active PCM from each
matching PID, terminates one flow, confirms the surviving flow remains alive
for 750 ms, cleans up both helpers, and verifies both cables again:

```powershell
./drivers/windows-audio/package/run-client-crash.ps1 -Independent -Execute -Cycles 1 -SmokeProbe ./drivers/windows-audio/target/debug/qpwgraph-audio-smoke.exe -EvidencePath ./drivers/windows-audio/target/candidate-current-source-r8-independent-client-crash-20260913-v2.json
```

All four rows passed: app render-target, app capture-target, relay
render-target, and relay capture-target. The retained JSON records four
successful target terminations, four live-survivor checks, recovery output,
the smoke probe hash
`50CA81C05FCB785DD33BE7D8B0C8B18C3B0467D97AEB46F141115D55D5F2C237`, and the installed SYS hash
`099F48379B892913BA6F585E253B07EA7DC7D9A0E48183BA579088A49D3259C2`.
This closes the bounded independent render/capture client-crash rows, but not
qpwgraph backend crash recovery or Driver Verifier evidence.

## Bounded sustained EOS/preroll — September 17

The installed candidate is unchanged: `oem24.inf`, INF DriverVer
`09/13/2026,11.8.28.7`, source commit `25ddbe6`, SYS SHA-256
`099F48379B892913BA6F585E253B07EA7DC7D9A0E48183BA579088A49D3259C2`
(re-verified with `Get-FileHash` before these runs). Windows 10 Pro build
19045. The probe binary was built from source commit `783a867` (sustained
mode plus parser-rejection unit tests); probe SHA-256
`776AB7AC47F0CDAE48A3DE036254B6EAD76939AC8486378FF7A1CD44CDED73D3`.
No boot, service, device, or default-endpoint setting was changed.

From repository root, with the prebuilt probe:

```powershell
./drivers/windows-audio/target/debug/qpwgraph-audio-ks-probe.exe --verify-sustained-eos
```

Defaults are 128 packets per cable, 20 s timeout, 1 preroll packet, and
960 final bytes. The first full run failed at `submit sustained packet 48`
with `0x8007045D` (`ERROR_IO_DEVICE`): the driver rejected one off-sequence
`SETWRITEPACKET` submit while the normal-priority probe chased the
free-running 10 ms timer. That rejection is the specified R7 late/skipped
behavior, not an EOS audio defect; no poisoned, replayed, or reordered
sample was observed. The failure transcript is retained unmodified at
`drivers/windows-audio/target/candidate-current-source-sustained-eos-live-run1-submit-rejected.log`
and must not be relabeled as a pass.

Two reruns passed on both cables (exit 0, ~1,400 ms per cable, 128 payload
packets with per-packet markers, 2 leading underflow-silence packets, exact
960-byte final prefix with poisoned tail rejected, 10 trailing silent
packets, preroll 1, explicit STOP):

- `drivers/windows-audio/target/candidate-current-source-sustained-eos-live-run2.log`
- `drivers/windows-audio/target/candidate-current-source-sustained-eos-live-run3.log`

A 16-packet run (`--verify-sustained-eos --packets 16`) also passed on both
cables
(`drivers/windows-audio/target/candidate-current-source-sustained-eos-16-live.log`),
and the 20-case `--verify-eos` plus both `u32::MAX` non-EOS ignored-length
checks passed again with this probe binary
(`drivers/windows-audio/target/candidate-current-source-eos-20case-live.log`).

This establishes bounded multi-packet EOS with preroll. It does not
establish a real 32-bit-counter wrap run, long-run-at-wrap behavior,
Driver Verifier, HLK, signing, or power-transition acceptance. The §18 EOS
and power rows stay open.

## EOS/lifecycle/timing rerun — September 17, 2026 (UTC)

Independent rerun on the unchanged installed candidate (`oem24.inf`, INF
DriverVer `09/13/2026,11.8.28.7`, source commit `25ddbe6`). SYS SHA-256
re-verified before the runs with `Get-FileHash`:
`099F48379B892913BA6F585E253B07EA7DC7D9A0E48183BA579088A49D3259C2`.
Windows 10 Pro build 19045.6466. Probe binary SHA-256
`776AB7AC47F0CDAE48A3DE036254B6EAD76939AC8486378FF7A1CD44CDED73D3`
(same binary as the sustained-EOS section above). No boot, service,
device, or default-endpoint setting was changed.

From repository root:

```powershell
./drivers/windows-audio/target/debug/qpwgraph-audio-ks-probe.exe --verify-eos
./drivers/windows-audio/target/debug/qpwgraph-audio-ks-probe.exe --verify-lifecycle
./drivers/windows-audio/target/debug/qpwgraph-audio-ks-probe.exe --verify-timing
```

All three exited 0:

- `--verify-eos`: 20 direct-KS cases passed on both cables (five
  two-packet layouts with final bytes 0/4/16/960/1920 plus five
  one-notification page-aligned layouts with final bytes
  0/4/16/2048/4096 per cable), poisoned tail 0 everywhere, at least 10
  later capture packets silent, explicit STOP passed; both
  `u32::MAX` non-EOS ignored-length checks accepted.
  Transcript:
  `drivers/windows-audio/target/candidate-current-source-eos-20case-rerun-20260917.log`.
- `--verify-lifecycle`: 17 start/pause/resume/stop cycles plus a reopen
  passed on all four endpoints; packet counts frozen during pause, reset
  on reopen; explicit STOP passed.
  Transcript:
  `drivers/windows-audio/target/candidate-current-source-lifecycle-rerun-20260917.log`.
- `--verify-timing`: presentation-position correlation passed on all
  four endpoints (71–73 position samples, 74 packets per endpoint, at
  most one-block error); pause and STOP passed.
  Transcript:
  `drivers/windows-audio/target/candidate-current-source-timing-rerun-20260917.log`.

This confirms the September 13 direct-KS EOS, lifecycle, and timing
evidence with the current probe binary. It does not establish a real
32-bit-counter wrap run, long-run/preroll-at-wrap behavior, controlled
active-stream sleep/resume, hibernate/reboot, backend crash recovery,
Verifier, HLK, signing, or Secure Boot acceptance. The §18 EOS and
power rows stay open.

## Sustained-EOS transient root cause and extended soak — September 17, 2026

Same installed candidate (`oem24.inf`, `09/13/2026,11.8.28.7`, source
`25ddbe6`, SYS SHA-256
`099F48379B892913BA6F585E253B07EA7DC7D9A0E48183BA579088A49D3259C2`,
re-verified before and after). Windows 10 Pro build 19045.6466.
Rebuilt probe binary SHA-256
`C44C7F143EBE89C51FF04093C8D3D7C84FB828884C9C2F0E7AE621328F2DB079`
(timer resolution + scheduling priority + render jump guard + capture
query-pair reconcile). No boot, service, device, or default-endpoint
setting was changed.

The earlier "transient off-sequence submit rejection" reproduced (first
128-packet run failed at `submit sustained packet 27` with `0x8007045D`;
transcript
`drivers/windows-audio/target/candidate-current-source-sustained-eos-default-20260917.log`,
retained as a failure) and was root-caused with an added render
completion-jump guard: the user-mode probe thread was descheduled for
12–23 ms mid-run (4 of 6 reps: jumps 68→71, 6→9, 59→61, 83→85 with
measured poll gaps; transcripts
`candidate-current-source-sustained-eos-diag{2,3,4,5}-20260917.log`),
during which the driver's free-running 10 ms timer consumed payload
slots as silence, so the next exact-successor submit was correctly
rejected as late. The driver behaved per design; the failures were host
scheduling artifacts, and no poisoned, replayed, or reordered sample
was observed in any run.

With the probe at elevated scheduling priority (as real audio clients
run), 6 of 6 128-packet reps passed clean on both cables (transcripts
`candidate-current-source-sustained-eos-prio1..6-20260917.log`), plus
extended soaks: 2048 packets per cable (~20.6 s per cable, exact
2060/2060 render/capture counts) and 8192 packets per cable (~82 s per
cable, exact 8204/8204 counts), all payloads oracle-verified with
preroll 1, 960-byte poisoned-tail final prefix, 10 trailing silent
packets, and explicit STOP (transcripts
`candidate-current-source-sustained-eos-p2048b-20260917.log` and
`candidate-current-source-sustained-eos-p8192-20260917.log`).

This extends bounded multi-packet EOS with preroll to 8192 packets per
cable. It does not establish a real 32-bit-counter wrap run,
long-run/preroll-at-wrap behavior, Verifier, HLK, signing, or
power-transition acceptance. The §18 EOS and power rows stay open.

## Version-bump upgrade — September 17, 2026

A pure version-bump package was staged from the installed candidate:
identical SYS bytes (SHA-256 `099F48...`, same test certificate
`30D29DBE073E11B6308872DA7170B3371BE6C037`, Inf2Cat 0 errors) with INF
DriverVer raised to `09/17/2026,11.8.28.8`. Full Smoke passed before
the upgrade on `oem24.inf`/`11.8.28.7`; the bumped package installed
live as `oem25.inf` with 0 errors and no reboot (no 3010), binding
`oem25.inf`/`11.8.28.8`/problem 0; full Smoke passed after; all six
default audio endpoints were byte-identical before and after (0 diffs).
The machine was then restored by uninstalling `oem25.inf` and
reinstalling the retained candidate, which reclaimed `oem24.inf` with
identical SYS hash and another green full Smoke; `oem25.inf` is absent
from the driver store and defaults still show 0 diffs. Transcripts:
`candidate-upgrade-smoke-before-20260917.log`,
`candidate-upgrade-install-20260917.log`,
`candidate-upgrade-smoke-after-20260917.log`,
`candidate-upgrade-restore-20260917.log`,
`candidate-upgrade-defaults-{before,after,restored}-20260917.txt`.
This closes the §10.1 version-bump upgrade sub-item. Reboot-required
upgrade behavior was not exercised (no 3010 occurred).

## Full GUI app-process kill — September 17, 2026

New test `live::gui_crash_during_active_stream_recovers`
(`PW_GRAPH_TEST_WINDOWS_GUI_CRASH=1`) kills the real
`target/release/qpwgraph-rs.exe` GUI (September 13 build, pre-seeded
config restores the route at startup) mid-stream instead of the minimal
crash host. Passed twice: route audible before kill (amplitude 0.0454 /
0.0458), kill remnant stuck at AppRender, GUI relaunch audible again
(0.0475 both runs), all four virtual endpoints enumerating. No GUI
process remained and the user's config files were verified byte-identical
to an independent backup after each run (0 diffs). Transcripts:
`drivers/windows-audio/target/candidate-gui-crash-20260917.log` and
`candidate-gui-crash-rerun-20260917.log`. This closes the §10.1 full
GUI app-process kill sub-item.

## Remaining release gates

The remaining EOS wrap/preroll cases, controlled active-stream sleep/resume,
hibernate/reboot, qpwgraph backend crash recovery, repeated upgrades/removals, Driver Verifier,
HLK, Microsoft production signing, and Secure Boot on a separate release-test
environment are not established by the basic cable checks. Follow
`WINDOWS_FEATURE_PARITY_PLAN.md`; do not mark
full parity or Test Mode disable readiness from these results alone.
