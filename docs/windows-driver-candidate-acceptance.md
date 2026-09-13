# Rust Windows driver candidate acceptance

Development validation on September 12–13, 2026. This supersedes the older
installed-package identity in `windows-driver-acceptance-baseline.md`, but
does not transfer that baseline's client/policy acceptance to this candidate.

## Installed package

- Driver source commit: `20da4d7` (ACX cleanup lifetime and single-packet mapping fixes).
- Windows 10 Pro; exact devnode `ROOT\DEVGEN\QPWGRAPH_AUDIO`.
- Published INF: `oem21.inf`; INF DriverVer `09/12/2026,11.8.52.696`.
- Service `qpwgraph_audio` running, devnode problem code 0.
- Existing development signer: `CN=QPWGraph Audio Test`, certificate
  `30D29DBE073E11B6308872DA7170B3371BE6C037`.
- SYS SHA-256 (signed staged package and installed file agree):
  `5FBD69AE4A4C0C19965958BB11EA1E25A111F9AD386F42E5EC2462B48F9F6EB4`.
- INF SHA-256:
  `5E9EBC46E4E503206CC268D7520B360D9DBA9AD7B4ED414AADCC6A3F8F57B57E`.
- CAT SHA-256:
  `F89369A2A28E814E07D08E9E0B8A8778F20A11509893A63514AACD1ECE7DC0ED`.

Signing, catalog membership verification, installation, active binding, and
four-role enumeration succeeded. Local transcripts (ignored build outputs):

- `drivers/windows-audio/target/candidate-20da4d7-install.log`
- `drivers/windows-audio/target/candidate-20da4d7-status.log`

Test-signing was verified **True**, Secure Boot **False**. No boot settings
or system default audio devices were changed. Keep Test Mode enabled on this
PC; full parity and readiness to disable Test Mode are **not complete**.

## Initial live smoke

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
ordinary WASAPI microphone consumer, not an OBS/browser/Discord UI session.

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
These are deterministic project-helper checks, not Chrome/VLC client acceptance.

## Recommended next validation batch

1. Verify driver packet/presentation timing and live EOS, then controlled
   active-stream crash/recovery and sleep/resume (the accidental sleep is not a pass).
2. Run driver-scoped Verifier with a recovery plan and retained crash/event
   evidence; the normal 300-cycle stress run does not substitute for this.
3. Complete MSIX/manual-override and Chrome/VLC/Discord client acceptance,
   then HLK and Microsoft signing/release gates on the required environments.

Keep the development PC in Test Mode throughout. Secure Boot validation belongs
on the separate release-test environment, not a boot change on this machine.

## Remaining release gates

Precise stream timing and EOS, controlled active-stream sleep/resume,
hibernate/reboot, client crashes, repeated upgrades/removals, Driver Verifier,
HLK, Microsoft production signing, Secure Boot on a separate release-test
environment, and remaining ordinary-client acceptance are not established by
the basic cable checks. Follow `WINDOWS_FEATURE_PARITY_PLAN.md`; do not mark
full parity or Test Mode disable readiness from these results alone.
