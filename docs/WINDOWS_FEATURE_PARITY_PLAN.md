# QPWGraph-RS — Windows Core Feature Completion Plan for LLM Agents

> Repository: `nglmercer/qpwgraph-rs`
>
> Target: complete core Linux/Windows feature parity while preserving the current working architecture, keeping the portable app usable without the optional driver, and making the optional Windows audio driver production-ready. Brand-specific third-party application checks are compatibility extras, not core gates.

---

## 0. Mission

Implement the remaining Windows features:

1. **Automatic per-application output switching**
2. **Production Microsoft-signed Windows audio driver pipeline**
3. **Driver Verifier / HLK / Secure Boot release validation**
4. **100% project-authored Rust Windows audio driver**
5. Finish the remaining Windows lifecycle and recovery validation gaps
6. Update stale documentation so it accurately reflects the implementation

The implementation must preserve all already-working Windows functionality.

---

## 0.1 Current verification snapshot (2026-09-13)

The repository-level Windows work is ahead of the original bootstrap wording:

- the driver source tree contains Rust runtime modules (`acx.rs`, `driver.rs`,
  `ffi.rs`, and `transport.rs`) plus the WDK-facing header wrapper; no
  project-authored `.c`, `.cc`, or `.cpp` runtime source remains;
- the WDK/ACX toolchain audit, ACX-enabled release build, package metadata,
  package staging, and Rust transport/EOS tests pass on the available PC;
- process-loopback recovery and application-relay restart/session probes pass
  without a virtual driver;
- an earlier isolated policy probe returned `E_INVALIDARG` and correctly
  demoted that policy instance to `ManualOnly`. September 13 end-to-end
  probes with an audio-producing Win32 helper passed all-three-role automatic
  switching, replacement-PID rebind, and exact restoration on rule removal
  and backend shutdown. The earlier error is not a blanket routing blocker;
  manual-override and unsupported-build fallback coverage remain open;
- Rust candidate `25ddbe6` adds shared EOS argument validation, the
  rollover-safe packet admission rule, and the monotonic scheduling counter.
  It is test-signed and installed as `oem24.inf`; the running devnode reports
  problem code 0 and installed and
  staged signed SYS hashes match;
- all four provider-owned roles enumerate on that candidate. Both cables
  pass the 1.5-second tone/isolation and stopped-render silence probe;
- the first candidate stress run passed 100 app cycles, 100 relay cycles,
  and 36 isolation cycles, then failed with zero silence-measurement frames
  across a host sleep/resume. Preserve that interrupted run as a failure,
  not a clean stress or sleep/resume acceptance result;
- a fresh September 13 ordinary stress matrix passed all 100 app, 100 relay,
  and 100 two-cable isolation cycles with the stricter two-capture silence
  probe. Its retained evidence is
  `drivers/windows-audio/target/candidate-current-source-r8-ordinary-stress-20260913.json`;
  that matrix intentionally did not restart AudioSrv or toggle the device.
  Separate idle-device disable/enable and AudioSrv restart checks also passed
  with fresh roles/cables verified afterward;
- the live isolated-effects probe exposed and verified a fix for registered
  effect outputs being rejected by playback connection checks. Noise-gate
  suppression and bypass restoration now pass on the candidate endpoints;
- all four shared-mode client clocks now pass initial/resumed/reset-start
  progression and stopped-position checks. Six owned-process crash/reopen
  cycles also pass (both render/capture clients die together). These do not
  close direct kernel timing/EOS, independent-client, or Verifier gates;
- the independent-client crash probe now passes one render-target and one
  capture-target termination on each cable: the surviving client stayed alive
  for 750 ms after its peer was terminated, and both cables recovered; qpwgraph
  backend-crash and Verifier evidence remain open;
- direct KS EOS now passes 20 live cases across both cables: ten two-packet
  cases and ten one-notification/page-aligned cases, including empty, partial,
  half-packet and full-packet endings. The probe also rejects skipped/late
  submissions and checks ordered PCM, poisoned-tail suppression, continued
  notifications, and explicit STOP. It also verifies on both render endpoints
  that a non-EOS packet accepts an oversized ignored EOS-length field;
- direct KS lifecycle now passes 17 start/pause/resume/stop cycles—more than
  twice the driver's 8-slot stream registry—and a reopen on all four endpoints,
  with packet counts frozen during pause and reset on reopen;
- direct KS presentation timing now passes on all four endpoints: the probe
  correlates `KSPROPERTY_RTAUDIO_PRESENTATION_POSITION` block positions with
  returned QPC timestamps, observed 71–72 position samples and 73–74 packets
  per endpoint, and measured at most one-frame error; pause and STOP also pass;
- the rollover-safe packet rule is integrated into the driver and covered by
  core tests, including `u32::MAX -> 0`; a real 32-bit counter-wrap run is not
  claimed because it would require billions of packets. Preroll/long-run wrap
  behavior and the remaining release gates stay open;
- remaining long-run EOS, Verifier, complete lifecycle, HLK, Secure Boot, and
  Microsoft signing are still release gates. Generic WASAPI probes define the
  core client contract; branded application matrices are optional.

See [candidate acceptance evidence](windows-driver-candidate-acceptance.md)
for package identity, retained failures, and subsequent validation. Historical
September 6 results remain separate from candidate-specific live evidence.

Development machine instruction: preserve Test Mode. Do not disable
test-signing or change Secure Boot on this PC. Record readiness for the user
only after the full requirements are verified; readiness is not permission
to change boot settings. Secure Boot release validation needs a separately
configured test environment while this development PC remains in Test Mode.

---

# 1. Non-negotiable architectural rules

## 1.1 Preserve the existing user-mode architecture

Keep these in Rust user mode:

- graph policy
- application route reconciliation
- process-loopback capture
- routing
- mixing
- resampling
- channel conversion
- gain
- effects
- RMS/peak metering
- relay transport
- relay policy
- persistence
- diagnostics
- endpoint/app identity resolution

The kernel driver must remain a **minimal virtual-audio transport/provider**.

Do not move DSP, relay, persistence, application policy, effect hosting, or graph ownership into kernel mode.

## 1.2 Portable mode must remain valid

The application must start and remain useful when the optional driver is not installed.

Portable mode must continue to support:

- Core Audio endpoint/session graph
- endpoint/session volume
- endpoint/session mute
- peak meters
- process-loopback RMS where supported
- physical capture/render routing
- render-loopback routing
- WinMM MIDI
- application relay using process loopback
- direct relay receive to physical output
- effects on user-mode-owned routes
- diagnostics

The driver must never become a hard startup dependency.

## 1.3 Separate read-only application capture from mutable application routing

Never collapse these concepts:

```text
read-only process capture
!=
mutating another application's output route
```

Ordinary application session:

```text
capture_readonly = true when process loopback is available
relay_source     = true when process loopback is available
meter_rms        = true when process loopback is available
mutable_route    = false
effects          = false for local rerendering
```

Application isolated on `QPWGraph Virtual Output`:

```text
capture_readonly = true
relay_source     = true
meter_rms        = true
mutable_route    = true
effects          = true
```

Never locally rerender an ordinary application while its original physical output remains active, because that creates:

```text
dry original
+
processed qpwgraph copy
```

---

# 2. Existing functionality that MUST NOT be reimplemented

Treat the following as landed foundations.

## 2.1 Core Audio

Already implemented:

- endpoint enumeration
- session enumeration
- graph creation
- endpoint/session volume
- endpoint/session mute
- native peak metering
- event-driven notifications
- physical capture -> render routing
- render monitor -> render routing
- WASAPI route transport
- device-loss signaling
- router counters

Do not rewrite this unless fixing a demonstrated bug.

## 2.2 Windows MIDI

Already implemented:

- WinMM enumeration
- mutable input -> output routing
- fan-out
- fan-in
- stable identity fallback
- patchbay persistence

Do not replace WinMM as part of this task.

## 2.3 User-mode router

Already implemented:

- fan-in
- fan-out
- mixing
- sample-rate conversion
- channel conversion
- software gain
- effect chains
- true RMS on owned PCM
- counters
- recovery signaling

Do not move any of this into the kernel driver.

## 2.4 Process-loopback

Already implemented:

- `ProcessLoopbackSource`
- process-tree include/exclude modes
- lifetime-safe `ActivateAudioInterfaceAsync`
- owned activation blob / `PROPVARIANT`
- completion handler lifetime
- async operation lifetime
- generation tracking
- capability probing
- process-capture manager
- identity re-verification
- per-app RMS
- read-only application relay
- restart handling

Do not reintroduce the old requirement that an application must already be on `QPWGraph Virtual Output` merely to capture or relay it.

## 2.5 Stable application identity

Already implemented:

- executable path hash
- executable name
- package family
- AUMID
- stable selectors
- PID reuse protection
- packaged app identity handling

Keep precedence approximately:

```text
AUMID
>
package family + executable identity
>
executable path hash
```

Never persist a PID.

## 2.6 Stable endpoint identity

Already implemented:

- `PKEY_AudioEndpoint_StableId` support where available
- current MMDevice ID fallback
- constrained friendly-name fallback
- provider-owned semantic role fallback
- qpwgraph service/parent/role verification

Do not return to friendly-name-only ownership checks.

## 2.7 Application route reconciler

Already implemented:

- persisted rules
- startup/refresh reconciliation
- stable selector -> current PID resolution
- isolation checks
- destination resolution
- capture readiness
- degraded/failure states
- effect restore
- gain restore
- transactional failure behavior
- restart recovery

Build on it instead of adding a second application-route state machine.

## 2.8 Current virtual endpoints

The existing driver path already targets four semantic endpoints:

```text
QPWGraph Virtual Output
QPWGraph Virtual Monitor
QPWGraph Relay Sink
QPWGraph Relay Microphone
```

The two logical cables must remain independent:

```text
Virtual Output -> Virtual Monitor
Relay Sink     -> Relay Microphone
```

No cross-talk is allowed.

---

# 3. Feature A — 100% project-authored Rust ACX driver

## 3.1 Goal

Remove project-authored C runtime implementation from the Windows audio driver.

The current runtime port is Rust-based. The driver source tree contains no
project-authored `.c`, `.cc`, or `.cpp` runtime implementation; the remaining
live acceptance work is tracked below and must pass before this feature is
called complete.

Final state must have:

```text
drivers/windows-audio/driver/src/
    driver.rs
    acx.rs
    ffi.rs
    transport.rs
    ...
```

and no project-authored C file implementing runtime driver logic.

Generated bindings, Microsoft import libraries, Windows headers, or build-generated glue are acceptable.

Project-authored runtime logic must be Rust.

## 3.2 Definition of “100% Rust driver”

Allowed:

- Rust
- generated Rust bindings
- bindgen output
- WDK import libraries
- Microsoft ACX/KMDF libraries
- build scripts
- generated metadata
- minimal generated compiler glue

Not allowed:

- project-authored `.c` / `.cpp` implementing:
  - device creation
  - circuit creation
  - pin configuration
  - stream callbacks
  - packet handling
  - timers
  - EOS
  - position reporting
  - power callbacks
  - stream lifecycle
  - cable logic

## 3.3 Port order

Do not port everything in one change.

### Phase R1 — Binding surface

Create or complete Rust definitions/wrappers for:

```text
ACX_DEVICE_CONFIG
ACX_CIRCUIT_CONFIG
ACX_PIN_CONFIG
ACX_DATAFORMAT_CONFIG
ACX_STREAM_CONFIG
ACX_STREAM_CALLBACKS
ACX_RT_STREAM_CALLBACKS
ACX_RTPACKET
ACX_STREAM_STATE
ACX_JACK_CONFIG
```

Wrap bindgen-hostile C macros with Rust functions where practical.

Example pattern:

```rust
unsafe fn init_pin_config(
    config: &mut ACX_PIN_CONFIG,
    pin_type: ACX_PIN_TYPE,
) {
    // Rust equivalent of the WDK init macro.
}
```

Do not duplicate undocumented layout guesses.

All layouts must come from the WDK headers used by the current build.

### Phase R2 — Driver/device bootstrap

Port to Rust:

- ACX driver initialization
- WDF device creation
- ACX device initialization
- device context
- prepare/release hardware
- D0 entry
- D0 exit

Acceptance:

```text
[x] driver loads
[x] device starts
[x] device stops (idle disable/enable)
[x] device remove works (single-cycle PnP remove + reinstall)
[x] no C runtime callback remains for these operations
```

Current evidence: the September 17, 2026 `oem24.inf` reinstall transcript
(`drivers/windows-audio/target/candidate-current-source-reinstall-20260917-uninstall.log`)
shows `pnputil /remove-device ROOT\DEVGEN\QPWGRAPH_AUDIO` reporting "Device
removed successfully", the driver package uninstalled and deleted,
provider-owned endpoint roles absent, and a green reinstall plus smoke
immediately after (`...-install2.log`, `...-smoke2.log`). Repeated
upgrades/removals stay open as a release gate. A same-day source audit found
zero project-authored `.c`/`.cpp` files under `drivers/windows-audio` and a
single `driver/build.rs` that only bindgen-generates Rust declarations plus
WDK-macro C glue (allowed by §3.2);
`drivers/windows-audio/driver/src/acx_wrapper.h` passes Rust callback
pointers opaquely (`void *prepare_hardware, ...`) and implements no callback
body, with every `Evt*` body in Rust (`driver.rs`, `acx.rs`).

### Phase R3 — Circuit creation

Port:

```text
AppRender circuit
AppMonitor circuit
RelayRender circuit
RelayCapture circuit
```

For each:

- circuit configuration
- component GUID
- circuit name
- endpoint category
- power callbacks
- role metadata contract

Acceptance:

```text
[x] four circuits enumerate
[x] semantic roles are preserved
[x] duplicate roles fail closed
[x] wrong flow/role combinations fail closed
```

Current evidence: duplicate provider roles resolve to
`VirtualAudioDriverHealth::AmbiguousRoles` (never Ready), covered by
`duplicate_friendly_roles_are_not_ready` and
`duplicate_verified_roles_are_not_ready`; wrong flow/role pairs are rejected
by `qpwgraph_endpoint_role_matches_flow` with all four valid and all four
invalid combinations covered by `wrong_flow_role_combinations_fail_closed`.
Command: `cargo test -p pw-graph-backend`.

### Phase R4 — Pins, formats, jacks

Port:

- host/device pin creation
- pin formats
- 48 kHz stereo PCM16 format
- jack descriptors
- endpoint categories

Do not widen format support during this port unless required by a failing client.

Initial canonical format:

```text
48,000 Hz
stereo
PCM16
shared-mode compatible
```

Acceptance:

```text
[x] render endpoints open shared mode
[x] capture endpoints open shared mode
[x] unsupported formats fail safely (direct 44.1 kHz KS rejection probe)
[x] jack metadata still appears (direct KSPROPERTY_JACK_DESCRIPTION probe)
```

Current evidence: `qpwgraph-audio-ks-probe --verify-formats` submits a valid
44.1 kHz stereo PCM16 pin request and receives a rejection on all four owned
endpoints. The canonical 48 kHz stereo PCM16 format remains the only format
advertised by the driver. The same endpoint pin returns one
`KSJACK_DESCRIPTION` with the expected
stereo channel map and configured connection/location fields on all four
circuits.

### Phase R5 — Stream lifecycle

Port:

- stream creation
- stream destruction
- prepare hardware
- release hardware
- run
- pause
- packet allocation
- packet free

Preserve the current one-render-producer / one-capture-consumer safety model per cable.

Acceptance:

```text
[x] open
[x] start
[x] pause
[x] resume
[x] stop
[x] reset (shared-mode smoke)
[x] reopen
[ ] no leaked stream context (17-cycle reuse passes; Verifier still required)
```

Current evidence: `qpwgraph-audio-ks-probe --verify-lifecycle` completes 17
direct KS lifecycle cycles—more than twice the driver's 8-slot stream
registry—and one reopen for each owned render/capture endpoint. The
shared-mode timing probe separately covers reset/start and stopped-position
behavior; stream-leak freedom remains a Verifier and long-run lifecycle gate.

### Phase R6 — Realtime packet callbacks

Port to Rust:

- set render packet
- get capture packet
- current packet
- presentation position
- timer pass
- packet scheduling
- QPC conversion
- monotonic position updates

Critical requirements:

```text
no allocation in realtime callback
no locks that can block indefinitely
no formatted logging
no panic across FFI
no user-mode dependency
no unbounded loops
```

All FFI entry points must be wrapped in a panic boundary or otherwise guarantee panic cannot unwind across the kernel ABI.

Acceptance evidence: `qpwgraph-audio-ks-probe --verify-timing` queries the
driver's direct presentation-position property on all four endpoints for at
least 750 ms, correlates block positions with the returned QPC timestamps,
checks monotonicity and packet progress, and verifies that pause freezes the
position before STOP. This is direct kernel timing evidence; it does not
replace long-run counter-wrap, preroll, Verifier, or HLK validation.

### Phase R7 — EOS

Preserve the current EOS behavior.

Requirements:

- final EOS packet length is applied
- only the declared final packet prefix is copied
- malformed lengths fail safely
- no circular-buffer replay after EOS
- notifications may continue without copying stale audio

Tests:

```text
empty EOS
partial EOS
full EOS
wrapping EOS
skipped EOS
malformed EOS
non-EOS packet length ignored per ACX contract
```

Current evidence: the direct KS probe passes empty/partial/full EOS in both
one-notification (single page-aligned packet) and two-notification layouts on
both owned cables, and live rejects late/skipped/malformed submissions. Both
render endpoints also accept a non-EOS packet whose ignored length is
`u32::MAX`; the shared core validator bounds the length only when EOS is set.
The shared packet-order helper and the monotonic scheduling counter cover the
`u32::MAX -> 0` transition in unit tests. A bounded sustained/preroll mode
(`--verify-sustained-eos`, 128 packets per cable, preroll 1) passed live on
the installed `25ddbe6` candidate on September 17, 2026, with one retained
transient off-sequence submit rejection; see
`windows-driver-candidate-acceptance.md`. Later that day the transient was
root-caused to 12–23 ms host scheduling stalls (render jump guard proof;
driver correctly rejected late submits after consuming the slots as
silence), the probe was hardened (elevated scheduling priority, 1 ms
timer resolution, capture query-pair reconcile), and extended soaks of
2048 and 8192 oracle-verified packets per cable passed on both cables
(6/6 clean 128-packet reps). The 20-case `--verify-eos`,
17-cycle `--verify-lifecycle`, and `--verify-timing` probes were re-run
against the same candidate later that day with the current probe binary
(exit 0 on all four endpoints); transcripts are recorded in the same
acceptance doc. A real long-run counter wrap and
preroll-at-wrap run remain release-gate work; do not mark this phase complete
from the bounded live cases alone.

### Phase R8 — Power callbacks

Port:

- device power callbacks
- circuit power callbacks
- idle-cable clearing

Rules:

```text
if no active stream:
    clear queued cable data

if active stream exists:
    do not clear under an active realtime producer/consumer
```

Implementation note: device D0, circuit power, stop, and device-release paths
use the active-stream guard before clearing either cable. The device-release
path was tightened in candidate `25ddbe6`; the live idle disable/enable and
AudioSrv recovery checks remain separate lifecycle evidence, and controlled
sleep/resume has not been run on this PC.

### Phase R9 — Remove C implementation

After all live tests pass:

```text
[x] confirm no project-authored C/C++ runtime source remains
[x] confirm the project-authored C runtime build path is absent
[x] ensure `cargo check --features acx`
[x] ensure package build
[x] ensure test-signed live install (`25ddbe6`, `oem24.inf`)
```

Do not delete the old C file before equivalent Rust live validation succeeds.

---

# 4. Feature B — Automatic per-application output switching

## 4.1 Goal

Allow qpwgraph to automatically move a selected application's persisted Windows output route to:

```text
QPWGraph Virtual Output
```

and optionally restore its prior route.

This feature is experimental because Windows does not expose a documented public Core Audio API for arbitrary third-party per-app output reassignment.

## 4.2 Mandatory safety architecture

Keep existing trait boundary:

```rust
pub trait AppRoutePolicy {
    fn support(&self) -> AppRoutePolicySupport;
    fn get_persisted_endpoint(...);
    fn set_persisted_endpoint(...);
}
```

Retain:

```text
UnsupportedAppRoutePolicy
```

Add:

```text
VerifiedAudioPolicyConfig
```

or equivalent.

Never remove the manual fallback.

## 4.3 Hard prohibition

Do NOT:

- scan COM vtables
- guess slot numbers dynamically
- call unknown methods “until one works”
- infer ABI from random examples without matching Windows build/interface
- make the feature default-on immediately
- persist raw PID as app identity

## 4.4 ABI implementation requirements

Create one isolated module, for example:

```text
crates/pw-graph-backend/src/windows/audio_policy_config.rs
```

It should contain:

- known CLSID/IID declarations
- exact interface vtable declaration
- version/build capability table
- explicit supported ABI versions
- runtime query/activation
- get persisted app endpoint
- set persisted app endpoint
- clear/restore persisted app endpoint
- HRESULT mapping
- safe fail-closed support detection

The rest of the backend must never call private COM interfaces directly.

## 4.5 Runtime support model

Suggested:

```rust
pub enum AppRoutePolicySupport {
    ManualOnly {
        reason: String,
    },
    Experimental {
        interface_version: String,
        os_build: u32,
    },
}
```

Add diagnostics:

```text
policy mode
Windows build
selected interface version
last HRESULT
last route operation
fallback reason
```

Do not expose sensitive process paths.

## 4.6 Configuration

Default:

```toml
[windows]
experimental_app_routing = false
```

Optional future setting:

```toml
[windows]
experimental_app_routing = true
restore_previous_app_route = true
```

The default must stay false until multi-build validation is complete.

## 4.7 Automatic isolation workflow

Target:

```text
application selected
    |
    v
resolve stable app identity
    |
    v
query current persisted output
    |
    v
save prior route in runtime transaction
    |
    v
set app output -> QPWGraph Virtual Output
    |
    v
wait for Core Audio session relationship to confirm isolation
    |
    v
start process loopback
    |
    v
activate router/effects/destination
```

Never assume the private API succeeded merely because the call returned success.

Confirm the session actually appears on `QPWGraph Virtual Output`.

## 4.8 Restore workflow

On route removal or application-rule disable:

```text
stop qpwgraph owned rerender
    |
    v
restore prior app endpoint if still safe
    |
    v
confirm current route
```

Do not restore if the user changed the app route manually after qpwgraph applied it.

Track ownership.

Suggested runtime transaction:

```rust
struct AutomaticAppRouteLease {
    selector: WindowsApplicationSelector,
    original_endpoint: Option<WindowsEndpointSelector>,
    applied_endpoint: WindowsEndpointSelector,
    generation: u64,
    owned: bool,
}
```

## 4.9 Race rules

Reverify application identity immediately before each private policy call.

Never trust:

```text
stale PID
display name
old session object
friendly name only
```

If PID changes:

```text
stop
resolve selector again
re-evaluate rule
```

## 4.10 Failover

If private policy is unavailable:

```text
ManualOnly
```

UI must show actionable instructions:

```text
Set this application's output to QPWGraph Virtual Output
in Windows Settings -> System -> Sound -> Volume mixer.
```

No route should become broken merely because automatic switching is unavailable.

## 4.11 Acceptance tests

Unit:

```text
[x] unsupported build -> ManualOnly
[x] unknown IID -> ManualOnly
[x] ABI mismatch -> ManualOnly (abi_mismatch_is_manual_only_on_the_validated_build)
[x] stale PID -> reject (dead_pid_is_rejected_as_a_stale_process_identity,
    live_pid_with_a_changed_identity_is_rejected_as_stale)
[x] display-name-only selector -> reject
[x] duplicate live selector match -> reject
    (duplicate_live_application_match_refuses_to_choose_a_pid)
[x] user manual override prevents unsafe restore (unit ownership model)
```

Live:

```text
[x] unpackaged Win32 app auto-moves (project tone helper)
[x] packaged MSIX app auto-moves (project packaged tone helper)
[x] app route confirms isolation (project tone helper)
[x] app effects activate only after isolation (helper live probe; ordinary-session guard regression)
[x] app restart re-applies route (replacement helper PID)
[x] qpwgraph restart reconciles safely (live backend drop/recreate with
    rule reinstall: route audible before and after, 0.22/0.22 amplitude;
    `backend_restart_reconciles_automatic_route_safely`; a full
    app-process restart with on-disk config reload remains unrun)
[x] disabling rule restores previous endpoint (all three roles)
[x] user manual override is preserved (live: external write through the
    same persisted store on all three roles; rule removal plus 6 s of
    refresh pumping left it untouched; `manual_override_of_automatic_route_is_preserved`)
[ ] unsupported Windows build falls back to manual mode
```

September 17, 2026 rerun on build 19045.6466 with the installed
`25ddbe6`/`oem24.inf` candidate re-confirmed the checked live rows above:
`experimental_application_route_rebinds_default_helper_and_restores`,
`isolated_application_route_rebinds_after_helper_restart`,
`isolated_application_effect_applies_and_bypass_restores_audio`, and
`experimental_application_route_restores_on_driver_shutdown` all passed,
and the rewritten
`live_policy_demotes_to_manual_only_for_process_without_audio` pins the
documented `E_INVALIDARG` demotion for a process with no audio session.
The new `manual_override_of_automatic_route_is_preserved` also passed:
after an external write moved all three helper roles off Virtual Output,
rule removal plus six seconds of refresh pumping wrote no restore, and
the rule re-applied cleanly afterwards. The new
`backend_restart_reconciles_automatic_route_safely` passed twice: route
audible before and after a backend drop/recreate with rule reinstall
(0.22/0.22 amplitude), with Drop-time restore of the live lease in
between.
Transcripts are retained under `drivers/windows-audio/target/` with the
`candidate-current-source-*-20260917.log` names. The remaining open row
above needs a second Windows build. The MSIX row closed the same day
with a project-owned packaged helper: after
`crates/windows-audio-test-tone/msix/build-test-msix.ps1` installed
`QPWGraph.TestTone_1.0.0.0_x64__0aet1w1jqgqs2` (one UAC prompt for
machine trust, since AppX deployment ignores per-user stores),
`PW_GRAPH_TEST_MSIX_APP_ROUTE=1 cargo test -p windows-audio-test-tone
--features relay-tests packaged_msix` passed: the packaged subject
(`QPWGraph.TestTone_0aet1w1jqgqs2!Tone`) moved to AppRender on all three
roles, the post-restart loopback measured 1 kHz at 0.1325 amplitude,
and rule removal restored the original endpoints. The helper activates
through `IApplicationActivationManager` because this host's alias stub
reports `E_APPLICATION_ACTIVATION_EXEC_FAILURE`; the package, machine
trust, and test certificate were removed afterwards
(`build-test-msix.ps1 -Uninstall -RemoveMachineTrust`), leaving zero
residue. The packaged-identity unit remains covered by
`PW_GRAPH_TEST_PACKAGED_IDENTITY=1 cargo test -p pw-graph-backend
packaged_process_identity` against the real
`Microsoft.Windows.StartMenuExperienceHost_cw5n1h2txyewy` subject on
build 19045.6466, with `from_pid` agreeing with an independent
fixed-buffer package probe on family name and AUMID.

Validate on several Windows 10/11 builds before considering default-on.

---

# 5. Feature C — Driver Verifier release gate

## 5.1 Goal

No public production driver release until Driver Verifier tests pass.

Add repository automation under:

```text
drivers/windows-audio/package/
drivers/windows-audio/tests/
```

Suggested scripts:

```text
enable-verifier.ps1
disable-verifier.ps1
run-driver-stress.ps1
collect-verifier-evidence.ps1
```

## 5.2 Safety requirement

Scripts must clearly distinguish:

```text
READ-ONLY
STATE-MUTATING
REBOOT-REQUIRING
```

Never enable Driver Verifier implicitly from a normal build.

Require an explicit argument/environment variable.

## 5.3 Verifier test categories

Use the appropriate checks for the driver, including where supported:

```text
Special Pool
Force IRQL checking
Pool Tracking
I/O Verification
Deadlock Detection
Security Checks
Miscellaneous Checks
WDF Verification
```

Do not enable unrelated verification flags blindly.

## 5.4 Driver stress matrix

Run under Verifier:

```text
100+ render open/start/stop/close cycles
100+ capture open/start/stop/close cycles
app cable active alone
relay cable active alone
both cables active
client crash during render
client crash during capture
qpwgraph process crash
AudioSrv restart
device disable/enable
driver uninstall/reinstall
driver upgrade
```

Record:

```text
bugcheck
Verifier violation
WDF violation
memory leak
handle leak
stuck stream
increasing nonpaged pool
increasing paged pool
CPU runaway
DPC/ISR anomalies
```

Current machine observation (read-only, September 13):
`drivers/windows-audio/target/candidate-current-source-r8-verifier-observation-20260913.json`
reports Verifier query settings available with `Verifier Flags: 0x00000000`
and `No drivers are currently verified`. This is not a clean Verifier run and
does not close the gate. The collection did not enable or disable Verifier,
change boot settings, or reboot the PC.

---

# 6. Feature D — HLK

## 6.1 Goal

Prepare for Windows Hardware Compatibility Program testing.

Repository can automate package preparation and evidence collection, but final HLK execution depends on a configured HLK Controller/Studio/test machine.

## 6.2 Add HLK documentation

Create:

```text
docs/windows-driver-hlk.md
```

Document:

- required Windows version
- test machine setup
- driver package to install
- expected four endpoints
- endpoint roles
- test-sign/preproduction-sign mode
- how to select audio device tests
- how to export HLK result package
- how to archive result evidence

## 6.3 Add HLK helper

Suggested:

```text
drivers/windows-audio/package/prepare-hlk.ps1
```

Responsibilities:

- validate release package
- validate catalog
- validate INF
- validate SYS
- print expected device instance
- print expected endpoint roles
- print exact package hash
- produce a manifest file for the HLK run

Do not pretend the script itself runs all HLK tests if it does not.

## 6.4 HLK definition of done

```text
[ ] relevant audio/device tests selected
[ ] all required tests pass
[ ] no unresolved errata requiring a code workaround
[ ] result package exported
[ ] result package hash archived
[ ] test machine build recorded
[ ] driver binary hash matches release candidate
```

---

# 7. Feature E — Secure Boot validation

## 7.1 Goal

Validate the driver with Secure Boot enabled before public release.

Required test tiers:

```text
development/test-sign tier
preproduction-sign tier
production Microsoft-sign tier
```

Do not conflate test-signing success with Secure Boot production compatibility.

## 7.2 Add release test script

Suggested:

```text
drivers/windows-audio/package/secure-boot-audit.ps1
```

Read-only output:

```text
Secure Boot enabled?
test-signing enabled?
driver package signer
catalog signer
installed provider
driver problem code
four endpoint roles
driver version
binary hash
```

## 7.3 Live Secure Boot tests

```text
[ ] clean boot
[ ] install package
[ ] four endpoints enumerate
[ ] app cable streams
[ ] relay cable streams
[ ] reboot
[ ] endpoints return
[ ] stream after reboot
[ ] uninstall
[ ] reboot
[ ] endpoints absent
```

No default audio device should be changed automatically.

---

# 8. Feature F — Production Microsoft signing pipeline

## 8.1 Goal

Make the repository capable of producing a final submission package and verifying the returned Microsoft-signed package.

The repository must NOT contain:

- private certificates
- EV token credentials
- Hardware Dev Center secrets
- refresh tokens
- signing passwords

## 8.2 Split release artifacts

Maintain two Windows release tiers.

### Portable

```text
qpwgraph-rs.exe
README
LICENSE
```

No driver required.

### Full

```text
qpwgraph-rs.exe
Microsoft-signed driver package
installer
uninstaller
README
LICENSE
driver notices
```

## 8.3 Release tooling

Add:

```text
drivers/windows-audio/package/build-release-driver.ps1
drivers/windows-audio/package/prepare-dashboard-submission.ps1
drivers/windows-audio/package/verify-returned-driver.ps1
packaging/windows/build-full-installer.ps1
```

## 8.4 Build release candidate

Pipeline:

```text
locked source commit
    |
    v
release Rust driver build
    |
    v
stamp INF
    |
    v
Inf2Cat
    |
    v
catalog verification
    |
    v
release manifest
    |
    v
hash every artifact
```

Output manifest example:

```json
{
  "git_commit": "...",
  "driver_version": "...",
  "sys_sha256": "...",
  "inf_sha256": "...",
  "cat_sha256": "...",
  "wdk_version": "...",
  "rust_version": "...",
  "llvm_version": "..."
}
```

## 8.5 Submission boundary

The build process may prepare the package.

Actual Microsoft submission requires organization/account credentials outside the repo.

The LLM implementation must:

- prepare deterministic submission artifacts
- document required account steps
- never invent credentials
- never store secrets
- verify returned signatures
- verify returned SYS/CAT correspond to the submitted build

## 8.6 Verify returned package

Check:

```text
[ ] Microsoft signature valid
[ ] expected publisher
[ ] INF included
[ ] SYS included
[ ] catalog valid
[ ] binary hash relationship documented
[ ] correct provider/service identity
[ ] four semantic endpoint roles
```

Then run Secure Boot smoke tests.

---

# 9. Feature G — Full Windows driver release workflow

## 9.1 Existing release workflow problem

The normal Windows release currently builds the portable ZIP only.

Add a separate full-driver release workflow.

Suggested:

```text
.github/workflows/windows-driver-release.yml
```

Do not force the portable release workflow to depend on privileged driver signing infrastructure.

## 9.2 Workflow stages

```text
source validation
unit tests
Windows workspace tests
eWDK ACX build
driver core tests
package validation
Verifier evidence check
HLK evidence check
Secure Boot evidence check
Microsoft signature verification
full installer build
release artifact generation
checksum generation
```

The workflow may require manually supplied external evidence artifacts for HLK/Microsoft signing.

Do not fake a green gate if evidence is absent.

---

# 10. Feature H — Remaining Windows lifecycle validation

## 10.1 Driver lifecycle

Close these live rows:

```text
[ ] sleep/resume
[ ] hibernate/resume if supported
[x] device disable/enable (idle-device transition; September 13 candidate)
[x] AudioSrv restart (fresh-client recovery; September 13 candidate)
[x] qpwgraph crash during active stream (live: backend-hosting process
    terminated mid-stream with an audible route; stuck AppRender remnant
    documented; fresh backend reconciled to an audible route with all four
    virtual endpoints enumerating;
    `backend_crash_during_active_stream_recovers`; September 17 full GUI
    app-process kill also passed twice via
    `gui_crash_during_active_stream_recovers`
    (`PW_GRAPH_TEST_WINDOWS_GUI_CRASH=1`): real release GUI killed
    mid-stream with an audible route (0.0454/0.0458), remnant stuck at
    AppRender, relaunch audible (0.0475), four endpoints enumerating, no
    GUI residue and user config byte-identical afterwards)
[x] render client crash (independent survivor probe; one cycle per cable)
[x] capture client crash (independent survivor probe; one cycle per cable)
[ ] reboot
[x] repeated install/uninstall (September 17: install-over-existing kept
    oem24.inf with 0 errors; uninstall removed devnode/store/endpoints;
    reinstall restored oem24.inf with identical SYS hash; Smoke passed
    before and after; transcripts
    `candidate-current-source-reinstall-20260917-*.log`)
[x] repeated upgrade (same-bits reinstall over the installed candidate
    twice with 0 errors and unchanged defaults; September 17 version-bump
    upgrade with distinct driver versions passed: identical-SYS package
    with INF DriverVer 09/17/2026,11.8.28.8 installed live as oem25.inf
    with 0 errors and no reboot, Smoke green before and after, six
    default endpoints byte-identical (0 diffs); oem24.inf/11.8.28.7
    restored afterwards with identical SYS hash and green Smoke)
```

## 10.2 Endpoint churn

Validate physical destination persistence:

```text
[ ] endpoint removed
[ ] route becomes degraded
[ ] same endpoint returns
[ ] stable selector resolves
[ ] route returns
```

Where `PKEY_AudioEndpoint_StableId` is absent:

- MMDevice ID may change
- constrained fallback must not attach to the wrong device

Mechanism note (September 2026): no software mechanism faithfully
removes the endpoint on this host. Disabling the SWD endpoint node
changes PnP state but the endpoint stays MMDevice-ACTIVE; disabling
the USB function is vetoed while the route streams (`Generic
failure`, `0x80131500`); removing the function deletes the PnP node
but leaves MMDevice state stale. Physical unplug/replug is therefore
the acceptance mechanism: `physical_destination_churn_restores_route`
(env `PW_GRAPH_TEST_WINDOWS_ENDPOINT_CHURN=1`) is operator-assisted,
watches endpoint presence (300 s each way), and fails safe on
timeout. A skipped run (timeout, no cleanup failure, endpoints
untouched) proves only the fail-safe, not churn.

Operator-run record (September 17, 2026, build 19045.6466): five
assisted runs, rows still open. Run 1 (UGREEN headphones) observed
removal, degrade to `DestinationMissing`, replug, same-MMDevice-id
return, and unchanged default, but failed the post-return amplitude
check (0.0032) because the wait used the monotonic lifetime
`frames_processed` counter and passed on stale pre-churn counts; it now
waits on `wait_for_route_frames_advancing` (delta past baseline).
Runs 2-5 then proved the UGREEN is Bluetooth (`BTHENUM`, run 1 was a
dropout) and the remaining `USB Audio Device` (C-Media VID_0D8C
PID_0012, root-hub port 2) could not be located physically despite six
guided attempts — it may be onboard/internal. This host therefore has
no known-removable USB render endpoint; rerun on a host with one. The
test also gained `PW_GRAPH_TEST_WINDOWS_CHURN_RENDER` to pin the churn
target by friendly-name substring. Transcripts: `/tmp/churn*.log`
(run 1), `/tmp/churn5.log` (pinned run, timed out waiting).

## 10.3 Application route destination loss

Live test:

```text
manually or automatically isolate app
    |
    v
route through effect
    |
    v
physical destination disappears
    |
    v
route enters degraded state
    |
    v
destination returns
    |
    v
route restores safely
```

No partial effect chain may remain.

---

# 11. Optional ecosystem compatibility

Core parity is defined by the generic WASAPI and deterministic project-owned
helpers already used by automated tests. Brand-specific application checks are
optional compatibility sampling only. They are not required dependencies,
release gates, or reasons to install software on a development machine.

If a distributor chooses to run an ecosystem matrix, keep its evidence
separate from core acceptance and verify the same public contracts:

```text
local playback remains unchanged for non-isolated application relay
only the selected process is relayed
stable selectors recover across process restart
Relay Microphone behaves as an ordinary shared-mode capture endpoint
```

---

# 12. Feature J — Windows external/module-backed effect host

## 12.1 Scope decision

Current Windows implementation rejects `module_path`.

If external/module-backed effects are part of the desired parity target, implement a Windows realtime module host.

If they are not part of the public feature contract, document them as intentionally unsupported.

Do not silently ignore module-backed effects.

## 12.2 Requirements if implemented

A module host must provide:

```text
stable ABI
realtime-safe process callback
parameter discovery
parameter update
bypass
lifecycle
error propagation
crash containment strategy
```

Never load arbitrary third-party code in kernel mode.

Modules stay in user mode.

---

# 13. CI requirements

## 13.1 Normal GitHub-hosted Windows CI

Keep:

```text
cargo fmt
cargo test
cargo clippy
Windows application feature combinations
driver core tests
package metadata validation
```

## 13.2 eWDK CI

Required when enabled:

```text
toolchain audit
ACX Rust compile
driver workspace tests
EOS tests
package build
package validation
```

After 100% Rust port:

```text
fail CI if project-authored C/C++ runtime source exists
```

Example guard:

```powershell
$forbidden = Get-ChildItem drivers/windows-audio/driver/src -Recurse -Include *.c,*.cc,*.cpp
if ($forbidden) {
    throw "Project-authored C/C++ runtime driver source remains."
}
```

Only add this guard after the Rust port is complete.

---

# 14. Diagnostics requirements

`Copy Windows audio report` must continue to be privacy-safe.

Include:

```text
OS build
Core Audio backend state
driver installed?
driver version
driver role health
endpoint stable selector type
process-loopback support
active process capture state
route reconciler states
automatic app-route policy support
private ABI version if enabled
last HRESULT
relay source/sink state
route counters
driver counters
```

Do not include:

```text
raw PCM
full executable paths
relay secrets
pairing PIN
private keys
tokens
arbitrary property blobs
```

---

# 15. Documentation updates

Update at least:

```text
docs/platform-parity.md
docs/features.md
docs/windows-driver-development.md
docs/windows-app-routing.md
docs/windows-process-loopback.md
docs/audio-router.md
docs/packaging.md
```

Fix stale statements that claim:

- application relay requires Virtual Output isolation
- per-app RMS requires Virtual Output isolation
- Relay Microphone is only scaffolding
- driver is 100% Rust while runtime C code remains

Documentation must distinguish:

```text
implemented
live validated
experimental
release-gated
unsupported
```

---

# 16. Security rules

## Kernel driver

Mandatory:

```text
no arbitrary privileged IOCTL
no user-controlled unchecked pointer
no unchecked length
no unbounded allocation in stream path
no blocking user-mode dependency
no panic across ABI
no formatted logging from RT callback
no stale PCM replay after stop
no cross-cable leakage
fail closed on malformed state
```

## Automatic app policy

Mandatory:

```text
opt-in by default
known ABI only
no vtable probing
no unknown slot calls
stable identity re-verification
ownership-aware restore
manual fallback
```

## Signing

Mandatory:

```text
no secrets in repo
no certificates committed
no token files committed
no CI logs containing secrets
verify returned Microsoft artifacts
```

---

# 17. Implementation sequence

Use this exact order unless a blocking dependency requires a change.

## PR 1 — Freeze current Windows acceptance baseline

- run existing tests
- record current working driver package hash
- record current four-endpoint smoke evidence
- update docs with exact baseline

## PR 2 — Rust ACX binding layer cleanup

- generate needed ACX Rust bindings
- add Rust init helpers
- no behavior change

## PR 3 — Port device/circuit setup to Rust

- device
- circuits
- roles
- power callbacks

## PR 4 — Port pin/format/jack setup to Rust

- pins
- PCM format
- jack descriptors

## PR 5 — Port stream lifecycle to Rust

- create
- prepare
- run
- pause
- destroy

## PR 6 — Port RT packet/timer/position callbacks to Rust

- render
- capture
- timers
- monotonic positions

## PR 7 — Port EOS and remove C runtime

- EOS
- old bridge removal
- project-authored C runtime guard

## PR 8 — Rust-driver live parity validation

- all four endpoints
- both cables
- isolation
- silence
- restart
- package lifecycle

## PR 9 — Experimental AudioPolicyConfig backend

- private ABI module
- exact interface declaration
- support detection
- default disabled

## PR 10 — Automatic isolation ownership model

- save prior app route
- apply Virtual Output
- confirm actual isolation
- safe restore
- user override protection

## PR 11 — Automatic route reconciler integration

- app restart
- qpwgraph restart
- packaged app
- destination restore

## PR 12 — Driver Verifier automation

- scripts
- evidence format
- stress matrix

## PR 13 — Lifecycle live tests

- sleep/resume
- disable/enable
- AudioSrv
- crash/restart
- reboot

## PR 14 — HLK preparation

- docs
- manifest
- helper scripts
- evidence requirements

## PR 15 — Secure Boot validation tooling

- read-only audit
- package verification
- live checklist

## PR 16 — Microsoft signing preparation

- release driver package
- submission manifest
- returned package verification

## PR 17 — Full Windows installer/release workflow

- portable/full split
- full installer
- signed driver validation
- checksums

## PR 18 — Backend and destination recovery

- qpwgraph backend crash recovery
- destination disappear/return
- physical endpoint churn

## PR 19 — Documentation parity cleanup

- remove stale statements
- classify each feature correctly

---

# 18. Definition of done

Do NOT call Windows full parity complete until all required rows below are true.

## User-mode

```text
[x] Core Audio graph
[x] volume/mute
[x] notifications
[x] device routing
[x] WinMM routing
[x] process-loopback
[x] per-app RMS
[x] single-app relay
[x] app route reconciler
[x] built-in effects
[x] stable app identity
[x] stable endpoint identity
[x] diagnostics
```

## Driver implementation

```text
[x] no project-authored C/C++ runtime driver code (static source audit)
[x] four ACX endpoints implemented in Rust
[x] two independent Rust PCM cables
[x] correct stream timing (bounded direct KS presentation-position check)
[ ] EOS correct
[ ] power transitions correct
[x] no cross-talk (current-candidate direct and 300-cycle isolation probes)
```

## Automatic app switching

```text
[x] private ABI isolated behind one module
[x] default disabled
[ ] supported-build detection
[x] automatic app -> Virtual Output
[x] actual isolation confirmation
[x] safe restore
[x] user manual override preserved
[x] manual fallback remains
```

Current evidence (build 19045.6466): the private ABI lives behind
`VerifiedAudioPolicyConfig` in `audio_policy_config.rs` (unit: ABI mismatch,
stale PID, duplicate selector); `Default` is disabled and the safe default
reports `ManualOnly` (unit: `unsupported_policy_is_actionable_and_safe`,
`disabled_policy_rejects_before_inspecting_process_identity`); unpackaged and
packaged-MSIX auto-move, isolation confirmation, safe restore, and
manual-override preservation are all live-verified
(`PW_GRAPH_TEST_WINDOWS_AUTO_APP_ROUTE=1`,
`PW_GRAPH_TEST_MSIX_APP_ROUTE=1`,
`PW_GRAPH_TEST_WINDOWS_APP_ROUTE_OVERRIDE=1 cargo test -p
windows-audio-test-tone --features relay-tests`); manual fallback stands via
`UnsupportedAppRoutePolicy` plus the live `E_INVALIDARG` demotion probe.
Supported-build detection stays open: Windows 10 verified, Windows 11
interface/build pending a second machine.

## Driver release

```text
[ ] Driver Verifier clean
[x] ordinary 300-cycle lifecycle stress clean
[ ] sleep/resume clean
[x] disable/enable clean (idle-device transition)
[ ] reboot clean
[ ] relevant HLK tests pass
[ ] Secure Boot validation pass
[ ] Microsoft production signature obtained
[ ] returned package signature verified
[ ] full installer built
```

September 17 Verifier attempt (build 19045.6466): enabling flags `0x33b`
for `qpwgraph_audio.sys` stages the settings (exit 2, "reboot required")
but a full unload/reload cycle of the demand-start driver did NOT attach
Verifier — `verifier /query` still reported "No drivers are currently
verified" after a green reinstall (oem24 reclaimed, endpoints OK). The
reboot requirement is real on this host, not a blanket message, so the
Verifier run needs a reboot-allowed window. Settings were reset the same
session (flags `0x0`, no drivers listed; machine-wide state pristine) and
the driver left healthy. Transcripts: `C:/tmp/verifier-cycle.log`,
`drivers/windows-audio/target/verifier-baseline-20260917.json`,
`drivers/windows-audio/target/verifier-postrun-20260917.json`.

## Core interoperability

```text
[x] generic WASAPI Relay Microphone
[x] generic process-loopback/application relay helper
[ ] physical destination disappear/return
[ ] physical endpoint churn persistence
```

---

# 19. Final target architecture

```text
                         Windows application
                                  |
                    automatic or manual isolation
                                  |
                                  v
                      QPWGraph Virtual Output
                                  |
                         Rust ACX driver
                                  |
                     QPWGraph Virtual Monitor
                                  |
             +--------------------+-------------------+
             |                                        |
             v                                        v
     ProcessLoopbackSource                      Rust user-mode router
             |                                        |
             |                              effects / gain / mix
             |                              RMS / resample / route
             |                                        |
             +--------------------+-------------------+
                                  |
                                  v
                          physical destination


Read-only ordinary app path:

Windows application -> normal output
        |
        +-> process loopback -> true RMS
        |
        +-> process loopback -> single-app relay


Relay receive:

remote peer
    |
    v
Rust relay
    |
    v
QPWGraph Relay Sink
    |
    v
100% Rust ACX driver
    |
    v
QPWGraph Relay Microphone
    |
    v
ordinary Windows shared-mode capture client
```

---

# 20. Final instruction to implementing LLM

When working on this repository:

1. **Inspect current code before changing anything.**
2. **Do not trust old roadmap text over current source.**
3. **Do not reimplement already-landed foundations.**
4. **Make one narrow feature change at a time.**
5. **Preserve portable no-driver startup.**
6. **Keep kernel code minimal.**
7. **Fail closed on identity, ABI, and endpoint ambiguity.**
8. **Never use guessed COM vtable slots.**
9. **Never claim release readiness from compile-only evidence.**
10. **Never claim Microsoft signing without verifying a Microsoft-signed returned package.**
11. **Do not mark live hardware rows complete from unit tests alone.**
12. **Record exact command, Windows build, driver version, package hash, and observed result for every live validation.**
13. **If a Windows behavior cannot be proven safely, leave the feature disabled and document the blocker.**

The project should reach full Windows parity incrementally:

```text
100% Rust driver
    ->
validated Rust driver
    ->
Verifier / lifecycle
    ->
HLK / Secure Boot
    ->
Microsoft-signed release
    ->
experimental automatic app switching
    ->
multi-build validation
    ->
supported automatic switching only if evidence is strong enough
```
