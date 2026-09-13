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

The fixed wall-clock measurement window was interrupted by host suspension.
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

## Remaining release gates

Precise stream timing and EOS, controlled active-stream sleep/resume,
hibernate/reboot, client crashes, repeated upgrades/removals, Driver Verifier,
HLK, Microsoft production signing, Secure Boot on a separate release-test
environment, and remaining ordinary-client acceptance are not established by
the basic cable checks. Follow `WINDOWS_FEATURE_PARITY_PLAN.md`; do not mark
full parity or Test Mode disable readiness from these results alone.
