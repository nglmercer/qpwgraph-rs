# Windows next-feature plan

Status snapshot: 2026-09-05. The repository now contains the P0 user-mode
implementation slices below. The ACX-enabled release driver links locally and
the WDK `stampinf`/`Inf2Cat` package stage passes with WDK 10.0.26100 and
released LLVM 21.1.2; live endpoint, HLK, and signing gates remain explicitly
open.

This document replaces the old “build everything from zero” roadmap with the next implementation steps after the Windows parity foundations landed.

### Verification snapshot

The repository-side evidence for this snapshot is complete:

- `cargo test --workspace --all-features --locked` passed on 2026-09-05,
  including the Windows backend, process-loopback integration tests, relay
  tests, effect tests, and doc tests;
- the WDK-initialized nested workspace passed `cargo test --workspace
  --locked` (3 driver transport tests and 11 core timing/transport tests);
- `--audit-toolchain` and `--build-package` passed with WDK 10.0.26100,
  Visual Studio 2022, and LLVM 21.1.2; INF stamping and Inf2Cat reported no
  errors or warnings, and the unsigned ACX package is staged under
  `drivers/windows-audio/target/qpwgraph-audio-package`.

The current machine has no installed QPWGraph endpoint, is not running an
elevated PowerShell session, and cannot read the boot configuration store.
Therefore the privileged test-signing/install/reboot step has not been
attempted; the live endpoint, client, Verifier, HLK, signing, Secure Boot, and
package lifecycle checks below remain unchecked until that external gate is
authorized and performed on a disposable Windows test image.

The main architectural rule stays unchanged:

> Keep routing, mixing, effects, gain, resampling, relay policy, persistence, and meters in Rust user mode. Keep the optional kernel driver as a minimal virtual-endpoint transport.

### Current implementation pass

- [x] ordinary render sessions expose capture-only process capabilities without becoming mutable graph edges;
- [x] single-application relay enumeration is independent of virtual-output isolation;
- [x] process-loopback RMS, route leases, and relay activations share one control-plane registry;
- [x] every PID-backed process-loopback activation re-verifies the stable selector immediately before opening the live PID;
- [x] route-capture readiness is keyed by stable selector plus live PID, and duplicate endpoint IDs/flow mismatches fail closed;
- [x] packaged identities use AUMID/package-family data when Windows exposes it;
- [x] endpoint selectors resolve stable ID -> current MMDevice ID -> unique friendly-name fallback;
- [x] persisted application routes reconcile on startup/refresh and migrate legacy destination selectors;
- [x] Windows audio report can be copied from the UI without exposing PCM, paths, or relay secrets;
- [x] virtual endpoint ownership now requires the qpwgraph service plus a provider-published semantic role property;
- [x] endpoint roles use typed INF `AddProperty` declarations, duplicate roles fail closed, and stable endpoint IDs remain case-sensitive;
- [x] provider-role smoke validation rejects missing, unknown, wrong-flow, and duplicate roles, while opt-in eWDK CI is required when enabled;
- [x] the driver smoke binary enumerates active endpoints, exercises shared-mode open/start/stop/reset, and can verify a non-silent round trip without changing defaults;
- [x] driver timing/discontinuity primitives, bounded `VirtualCable` transport, and fail-closed ACX binding gates are present;
- [x] the opt-in ACX bridge now contains app render/monitor and relay sink/microphone circuit pairs, timer-driven RT callbacks, and two independent Rust-owned bounded PCM cables behind the ACX feature;
- [x] ACX bridge pins now publish device-side jack descriptors alongside their endpoint categories;
- [x] complete persisted application-route effect instances restore transactionally with parameters and bypass/enabled state, while legacy ID-only chains fail closed;
- [x] rejected Windows route/effect/gain replacements restore the prior router tables and control-plane ownership instead of leaving half-applied state;
- [x] Windows rejects module-backed effects until a matching realtime host exists, and the ACX bridge enforces one render producer and one capture consumer per bounded cable;
- [ ] a real enumerated/streaming endpoint, relay microphone, installer-ready package, and HLK/signing evidence still require an eWDK/Windows validation pass.

---

## 1. What is already landed

Do not reimplement these items.

### User-mode Windows audio

- [x] Core Audio endpoint/session graph
- [x] endpoint/session volume and mute
- [x] native peak metering
- [x] event-driven endpoint/session notifications
- [x] physical capture -> render routing
- [x] render-loopback -> render routing
- [x] user-mode PCM router
- [x] mixing / fan-in
- [x] fan-out
- [x] sample-rate conversion
- [x] channel conversion
- [x] software gain
- [x] effect chains
- [x] true RMS for PCM owned by qpwgraph
- [x] route counters / diagnostics
- [x] device-loss signaling back to the control plane

### Windows MIDI

- [x] WinMM enumeration
- [x] mutable routing
- [x] fan-out
- [x] fan-in
- [x] stable device identity fallback
- [x] patchbay persistence

### Process loopback foundations

- [x] `ProcessLoopbackSource`
- [x] `AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK`
- [x] include/exclude process-tree modes
- [x] async activation lifetime ownership
- [x] COM-task-allocated activation parameters
- [x] owned `VT_BLOB` / `PROPVARIANT`
- [x] completion handler lifetime
- [x] activation operation lifetime
- [x] generation counter
- [x] bounded router ring
- [x] capability probe
- [x] deterministic Windows test-tone helper
- [x] opt-in process-loopback integration test
- [x] process-loopback source usable by the router
- [x] process-loopback source usable by relay code

### Application identity / policy foundations

- [x] `ProcessIdentity`
- [x] executable-path hashing
- [x] stable selector concept
- [x] persisted `WindowsApplicationRoute` schema
- [x] no persisted PID
- [x] isolated `AppRoutePolicy`
- [x] safe `UnsupportedAppRoutePolicy`
- [x] manual Windows Volume Mixer fallback
- [x] experimental routing config switch defaults to false

### Optional Windows driver foundations

- [x] nested `drivers/windows-audio` workspace
- [x] KMDF Rust bootstrap
- [x] no-std driver crate
- [x] `windows-drivers-rs` integration
- [x] allocation-free ring core
- [x] SPSC ring implementation
- [x] underflow/overflow/discontinuity counters
- [x] INF/INX scaffold
- [x] install/uninstall guards
- [x] package manifest
- [x] smoke-test crate
- [x] eWDK `xtask`
- [x] opt-in self-hosted eWDK CI job
- [x] fail-closed `EvtDeviceAdd`

### Virtual endpoint foundations

- [x] semantic roles:
  - `AppRender`
  - `AppMonitor`
  - `RelayRender`
  - `RelayCapture`
- [x] driver-health model
- [x] relay virtual-microphone selector plumbing

---

## 2. Important architecture correction

The current implementation couples these two different concepts too tightly:

1. **capturing a process**
2. **rerouting a process**

They must be separated.

Windows process-loopback capture can capture the render streams of a target PID/process tree without requiring that process to render to `QPWGraph Virtual Output`.

Therefore:

```text
read-only process capture
    !=
mutable application route
```

This distinction unlocks useful features before the driver is finished.

## Capture-only uses

These do **not** create a second local audible path:

```text
application
    -> Windows audio engine / normal speakers

        +-> process loopback
              -> RMS meter

        +-> process loopback
              -> relay encoder
              -> remote peer
```

These features do not require virtual-output isolation:

- single-application relay source;
- true per-application RMS;
- process-audio diagnostics;
- optional process recording/debug tooling.

## Mutable/rerendered uses

These **do** require isolation:

```text
application
    -> process loopback
    -> qpwgraph effects/router
    -> speakers
```

Without moving the application's original output away from the physical speaker, the user could hear:

```text
dry original
+
processed qpwgraph copy
```

Therefore these features remain gated on proven isolation:

- per-app local rerouting;
- per-app effects rendered locally;
- patchbay application routes;
- application -> arbitrary endpoint links.

### New capability split

Add an explicit model instead of using one `node_supports_routing` decision for every process feature.

Suggested shape:

```rust
pub struct ProcessAudioCapabilities {
    pub capture_readonly: bool,
    pub relay_source: bool,
    pub meter_peak: bool,
    pub meter_rms: bool,
    pub mutable_route: bool,
    pub effects: bool,
}
```

Rules:

```text
ordinary render session:
    capture_readonly = process-loopback capability
    relay_source     = process-loopback capability
    meter_rms        = process-loopback capability
    mutable_route    = false
    effects          = false

session on QPWGraph Virtual Output:
    capture_readonly = true
    relay_source     = true
    meter_rms        = true
    mutable_route    = true
    effects          = true
```

This implementation pass has now landed this capability split; the remaining
work in this section is live Windows acceptance and regression coverage.

---

# 3. Priority P0 — Finish driver-independent process features

Do these before spending the whole development cycle on ACX.

---

## P0.1 Ungate single-application relay

### Current state

The relay source adapter now enumerates active render sessions independently of
`QPWGraph Virtual Output`. Live process-loopback activation and identity
verification still need the Windows acceptance checks below, but the user-mode
path no longer depends on the virtual driver.

### New behavior

Offer eligible active render sessions as relay sources even on normal physical endpoints.

Flow:

```text
normal application
    -> existing Windows output
    -> process-loopback capture
    -> relay
    -> remote peer
```

The application's local playback remains unchanged.

### Files

Likely:

```text
crates/pw-graph-backend/src/windows/worker.rs
crates/pw-graph-backend/src/windows/driver.rs
crates/pw-graph-backend/src/windows_relay.rs
crates/pw-graph-slint/src/bridge/relay.rs
crates/pw-graph-slint/src/source.rs
```

### Required changes

- Enumerate relay-capable application sources independently of virtual-output isolation.
- Resolve stable selector -> current live PID at activation time.
- Verify identity immediately before opening process loopback.
- Keep existing session-isolated proof only for mutable graph routing.
- On process disappearance:
  - stop only the relay capture worker;
  - retain authenticated relay control session;
  - mark source unavailable;
  - restart if the same stable application selector returns.
- Never fall back to a different PID.

### Acceptance

```text
[ ] Chrome/Firefox/VLC on normal speakers appears as an application relay source
[ ] starting app relay does not change the application's local output
[x] only target-process audio reaches the relay (opt-in two-helper process-loopback isolation smoke test passed locally)
[x] another application on the same endpoint is excluded (opt-in two-helper process-loopback isolation smoke test passed locally)
[x] child-process mode behaves as documented (opt-in child-tree process-loopback smoke test passed locally)
[x] target exit does not kill the relay control session (opt-in local host/client smoke test passed locally)
[x] target restart safely resolves a new PID (opt-in helper smoke test passed locally)
```

---

## P0.2 True RMS for ordinary application sessions

### Goal

Use process loopback as a read-only meter source.

Current native session meter remains the cheap peak fallback.

### Meter policy

#### `off`

```text
no process-loopback meter
```

#### `on-demand`

Open process loopback only when:

- the node is visible/selected and requests RMS;
- diagnostics explicitly requests it;
- another consumer already owns process capture.

#### `always`

Keep process-loopback RMS for eligible active sessions, subject to a sane worker limit.

### Rules

- Do not create a graph route merely to meter.
- Do not expose the capture-only source as a mutable graph edge.
- Native `IAudioMeterInformation` stays the fallback.
- If process loopback fails:
  - keep native peak if available;
  - set RMS unavailable;
  - report the HRESULT/reason.

### Acceptance

```text
[x] ordinary app session can report true RMS without virtual driver (opt-in helper smoke test passed locally)
[ ] audible output is unchanged
[x] peak fallback survives process-loopback failure (opt-in live native-peak fallback smoke test passed locally with fault-injected process-loopback activation)
[x] meter policy controls worker lifetime (opt-in helper smoke test passed locally)
[x] process exit closes capture (opt-in live smoke test passed locally)
```

---

## P0.3 Add a shared process-capture control layer

The new relay + RMS use cases can otherwise create duplicate process-loopback activations for the same PID.

Add:

```text
crates/pw-graph-backend/src/windows/process_capture.rs
```

Suggested responsibilities:

```rust
ProcessCaptureManager
ProcessCaptureKey
ProcessCaptureConsumer
ProcessCaptureState
```

Key:

```text
stable application selector
+
live PID
+
process generation
+
include/exclude mode
```

Consumers:

```text
Meter
Relay
OwnedRoute
Diagnostics
```

The manager does not need to share a single `AudioSource` object between unrelated realtime graphs immediately.

At minimum it should:

- know active captures;
- serialize activation/restart decisions;
- prevent accidental stale PID reuse by re-reading the live process identity immediately before activation;
- expose health/state;
- provide one place for backoff and capability invalidation.

A later optimization may fan out one capture stream to several consumers.

---

# 4. Priority P0 — Strengthen stable identity

The user-mode identity foundation is implemented; the remaining checks verify
that the Windows APIs return durable values across the supported app classes.

---

## P0.4 Populate packaged-app identities

`ProcessIdentity::from_pid` queries the package identity APIs while the process
handle is open and provides:

```text
executable_path_hash
executable_name
package_family_name
app_user_model_id
```

The schema already reserves:

```text
package_family_name
app_user_model_id
```

Populate them when available.

Relevant Windows APIs include:

```text
GetPackageFamilyName
GetApplicationUserModelId
```

or an equivalent supported package identity path.

### Matching preference

Prefer:

```text
AUMID
    >
package family + executable identity
    >
executable path hash
```

Display name remains a hint only.

### Why

This matters for:

- Store/MSIX apps;
- host processes;
- applications whose executable path changes across updates;
- multiple packaged apps sharing framework/runtime processes.

### Acceptance

```text
[x] unpackaged Win32 app survives restart (reconciler + opt-in helper restart coverage passed locally)
[ ] packaged app survives restart/update when its stable app identity remains
[x] PID reuse never activates an unrelated app (identity/reconciler tests passed locally)
[x] display-name-only selector never activates automatically (selector test passed locally)
```

---

## P0.5 Adopt `PKEY_AudioEndpoint_StableId`

Microsoft now documents `PKEY_AudioEndpoint_StableId` as an opaque endpoint identifier Windows attempts to preserve across OS and audio-driver updates.

Use it when available.

Add a stable endpoint selector:

```rust
pub struct WindowsEndpointSelector {
    pub stable_id: Option<String>,
    pub current_mmdevice_id: Option<String>,
    pub friendly_name: Option<String>,
    pub data_flow: AudioFlow,
}
```

Resolution order:

```text
1. PKEY_AudioEndpoint_StableId
2. current IMMDevice ID
3. constrained friendly-name fallback
```

Never use friendly name alone when several devices match.

### Apply to

- physical destination persistence;
- relay selected source/sink;
- application-route destination;
- optional virtual driver endpoints;
- default-device reconciliation diagnostics.

---

# 5. Priority P0 — Finish application-route persistence

The dedicated reconciler now owns startup/refresh state, capture leases,
destination-selector migration, direct isolated-session route restore, and
transactional restoration of complete effect instances.

The current policy is explicit and fail-closed: a route with legacy ID-only
effect data, an unavailable effect host, or any processor/link/gain failure
enters `EffectRestoreFailed` and no partial route is left installed. Complete
effect instances preserve their parameters, enabled/bypass state, and stable
instance identity across configuration reloads.

---

## P0.6 Build the persisted application-route reconciler

Add a dedicated reconciler rather than spreading restore logic through refresh code.

Suggested location:

```text
crates/pw-graph-backend/src/windows/app_route_reconciler.rs
```

or, if it belongs above backend policy:

```text
crates/pw-graph-app-core/src/windows_app_routes.rs
```

### State machine

```text
Configured
    |
    v
WaitingForApplication
    |
    v
WaitingForIsolation
    |
    v
ActivatingCapture
    |
    v
ResolvingDestination
    |
    v
Active
```

Failure states:

```text
UnsupportedOS
IdentityMismatch
VirtualDriverMissing
VirtualOutputNotSelected
DestinationMissing
ProcessCaptureFailed
EffectRestoreFailed
Degraded
```

### Rules

A rule may become `Active` only if:

1. stable app selector matches a live session;
2. live PID identity is reverified;
3. local rerouting requires isolation and isolation is proven;
4. destination selector resolves uniquely;
5. process loopback starts;
6. route table is accepted transactionally;
7. every complete effect instance restores successfully, including its
   parameters and enabled/bypass state.

### Retry triggers

Retry on:

```text
session created
session state changed
endpoint added
endpoint removed
default endpoint changed
driver endpoint appeared
audio service recovery
explicit user retry
```

Do not poll aggressively.

### Acceptance

```text
[x] saved route survives app restart (reconciler restart transition test passed locally)
[x] saved route survives qpwgraph restart (configuration file round-trip test passed locally)
[x] missing app shows WaitingForApplication (reconciler test passed locally)
[x] missing endpoint shows degraded state (reconciler test passed locally)
[x] returning endpoint restores safely (reconciler test passed locally)
[x] reused PID never matches by number (reconciler identity test passed locally)
[x] complete effect instances restore transactionally or fail closed
```

---

# 6. Priority P0 — Finish support diagnostics UI

The bounded backend report and a Windows diagnostics rail action are now wired;
the live report still needs validation on a Windows audio system.

---

## P0.7 Add “Copy Windows audio report”

UI:

```text
Help
  -> Diagnostics
      -> Copy Windows audio report
```

Report should include:

```text
OS/build
Core Audio backend state
virtual-driver health
virtual endpoint roles
process-loopback support
active process captures
selector hash / safe application name
PID + generation for live diagnostics only
route destination stable ID
route metrics
last HRESULT
last route fault
relay source/sink state
device-loss/restart counters
```

Do not include:

```text
raw PCM
full executable paths
relay secrets
pairing PINs
credentials
opaque property-store blobs
```

---

# 7. Priority P1 — Implement the actual Rust ACX endpoint

The default driver package is intentionally fail-closed:

```text
EvtDeviceAdd -> STATUS_NOT_SUPPORTED
```

The opt-in `acx` build now contains the app and relay endpoint transactions in
the C-side ACX bridge, while the Rust entry point keeps the opaque ACX ABI
isolated. The release build and unsigned package stage are verified locally;
the remaining blocker is the test-signed load, endpoint enumeration, and
streaming validation pass.

---

## P1.1 ACX binding feasibility

Before implementing circuits, prove that the eWDK environment can generate/link the ACX APIs required by the selected sample architecture.

Run the repository preflight from the nested workspace:

    Push-Location drivers/windows-audio
    cargo run -p qpwgraph-audio-xtask --locked -- --audit-toolchain
    Pop-Location

It reports WDKContentRoot, versioned KM CRT headers, acx.h, the target
architecture's `acxstub.lib`, cl.exe, msbuild.exe, and clang.exe separately.
A failed preflight is expected to
leave the bootstrap entry point fail-closed; it must not be treated as a
successful ACX build.

Once that preflight passes, the opt-in binding compilation is:

    Push-Location drivers/windows-audio
    cargo check -p qpwgraph-audio --features acx --locked
    Pop-Location

This is a header/ABI and feature-bridge compilation gate. The release package
stage is:

    Push-Location drivers/windows-audio
    cargo run -p qpwgraph-audio-xtask --locked -- --build-package
    Pop-Location

It builds the ACX `.sys`, stamps the INF, generates the catalog, and stages an
unsigned package under `target/qpwgraph-audio-package`. It does not close the
endpoint milestone or authorize installing the package.

Create:

```text
drivers/windows-audio/driver/src/ffi.rs
drivers/windows-audio/driver/src/acx.rs
```

Keep raw bindings isolated.

The opt-in bridge exercises `AcxDeviceInitialize`, `AcxCircuitCreate`,
`AcxPinCreate`, `ACX_DATAFORMAT_CONFIG_INIT_KS`/`AcxDataFormatCreate`,
`AcxStreamInitAssignAcxStreamCallbacks`,
`AcxStreamInitAssignAcxRtStreamCallbacks`, and `AcxRtStreamCreate` behind the
`acx` feature. The C bridge contains named 48 kHz stereo app render/monitor
and relay sink/microphone circuit pairs, bounded RT packet allocation,
timer-driven packet completion, and monotonic presentation position. Each
pair crosses the narrow C ABI into its own Rust `SpscSampleRing`, and capture
underflow is filled with silence. The small bindgen wrappers remain narrow ABI
boundaries; the production bridge now also links as a release kernel driver.
Test-signed Windows endpoint and stream tests are still required before the
package is considered ready.

The opt-in eWDK CI job runs both `--audit-toolchain` and
`cargo check -p qpwgraph-audio --features acx` before attempting the package
build. On 2026-09-05 both commands passed locally with WDK 10.0.26100 and
LLVM 21.1.2. LLVM 22 currently produces invalid WDK layout bindings, so the
audit accepts released LLVM 17--21 and reports newer versions as unsupported.

### Preferred approach

1. use `windows-drivers-rs` for WDF/KMDF;
2. generate required ACX headers locally with bindgen if `wdk-sys` does not expose them;
3. manually wrap only bindgen-hostile macros/constants;
4. keep safe Rust wrappers narrow.

Do not add a C++ production driver merely because ACX headers require custom binding work.

### Gate

Do not claim four-endpoint readiness until this succeeds:

```text
[x] ACX device initialization compiles
[x] ACX circuit creation compiles
[x] ACX pin/format config compiles
[x] ACX stream callback config compiles
[x] ACX-enabled release driver links
[x] unsigned INF/CAT package stages
[ ] test-signed package loads
```

---

## P1.2 First endpoint: QPWGraph Virtual Output

The feature-gated implementation creates the app render/monitor pair and the
independent relay sink/microphone pair, and connects each pair to its own
bounded Rust cable. The acceptance conditions below remain unchecked until a
real eWDK/test-signed Windows pass.

The first live validation target is the app render/monitor pair; relay
endpoint acceptance depends on the same stream, verifier, and package gates.

Target:

```text
QPWGraph Virtual Output
```

It must appear in:

```text
Device Manager
Windows Sound
IMMDeviceEnumerator
```

### Required minimum

- shared-mode WASAPI open;
- 48 kHz stereo;
- float32 or PCM16;
- start;
- stop;
- reopen;
- disable/enable;
- unload.

The relay circuits are present in the feature-gated source, but no endpoint
role is considered ready until the first render/monitor cable has passed the
live stream and verifier gates as well.

### Exit condition

A deterministic user-mode tone can render to the endpoint for several minutes without:

- bugcheck;
- verifier violation;
- runaway CPU;
- increasing kernel memory;
- stuck stream on stop.

---

# 8. Priority P1 — Add the missing driver timing model

The ring core is implemented, but a production audio driver also needs correct stream timing.

This should be an explicit milestone, not an implicit detail of “virtual cable”.

Add a virtual clock/position model.

Suggested pure-Rust core types:

```rust
StreamClock
StreamPosition
PacketTimeline
StreamState
```

Track:

```text
frames started
frames presented
frames captured
QPC/start timestamp
packet number
discontinuity generation
running/stopped state
```

Requirements:

- monotonic positions;
- reset only at documented stream transitions;
- no time going backwards after pause/resume;
- render and capture sides agree on the same virtual clock domain;
- silence underflow still advances capture time;
- overflow does not rewind position.

Add pure Rust tests before wiring ACX callbacks.

---

# 9. Priority P1 — Virtual cable A

The driver-independent transport contract now exists in
`drivers/windows-audio/core/src/lib.rs` as `VirtualCable<N>`. It enforces one
mixed render producer and one capture consumer, couples the bounded SPSC ring
to monotonic render/capture clocks, records packet QPC/discontinuity state, and
tests drop-newest overflow (including whole-frame admission for an unaligned
sample capacity) plus silence-on-underflow. The opt-in ACX adapter now connects
the PCM16 packet callbacks to the Rust SPSC cable; the live pin, clock, and
underflow behavior still needs Windows validation.

After one render endpoint and the timing model work:

```text
QPWGraph Virtual Output
        |
        v
kernel bounded ring
        |
        v
QPWGraph Virtual Monitor
```

### Driver pair state

Suggested:

```rust
struct VirtualCable<const N: usize> {
    ring: SpscSampleRing<N>,
    render_clock: StreamClock,
    capture_clock: StreamClock,
    active_render_streams: AtomicU32,
    active_capture_streams: AtomicU32,
}
```

If ACX permits more than one stream per endpoint, define the policy explicitly.

Initial acceptable policy:

```text
Windows audio engine performs shared-mode client mixing
driver receives one mixed render stream
driver exposes one capture stream
```

Do not create an uncontrolled N-producer ring.

### Underflow

```text
capture -> silence
counter++
discontinuity generation++
time continues
```

### Overflow

Keep the current documented policy:

```text
drop incoming newest tail
counter++
discontinuity generation++
```

Revisit only if live latency tests show a better policy is necessary.

---

# 10. Priority P1 — Robust virtual endpoint identity

The operational worker no longer treats friendly names as ownership proof.
`from_endpoint_names` remains a compatibility/scaffolding helper for tests and
diagnostics, but live endpoint classification requires provider-owned
properties.

A user-renamed device or another device with the same name must not be mistaken for the qpwgraph driver.

---

## P1.3 Driver-owned identity

Expose enough identity for user mode to prove ownership.

Use a combination of:

```text
PnP instance / hardware identity
PKEY_AudioEndpoint_StableId
driver provider/service identity where available
semantic role
```

Friendly name is display-only.

Suggested classification result:

```rust
pub struct QpwVirtualEndpointIdentity {
    pub role: QpwVirtualEndpointRole,
    pub stable_endpoint_id: Option<String>,
    pub mmdevice_id: String,
    pub driver_version: Option<String>,
}
```

The provider contract used by the worker is:

```text
DEVPKEY_Device_Service == qpwgraph_audio
PKEY_QPWGraph_EndpointRole == app-render | app-monitor | relay-render | relay-capture
```

The current key declaration is `{3c8e8ef9-1f7f-4fcb-9c36-4a7e19f36d12},2`.

The role key is project-owned and is intentionally separate from the display
name. The worker also records `PKEY_AudioEndpoint_StableId` and the driver
version when Windows exposes them. The ACX endpoint still has to publish this
contract on each endpoint interface. The current INF template uses `AddProperty`
sections for all four role values; until a live ACX install exposes and verifies
both properties on all four endpoints, the health state stays
`NotInstalled`/`Incomplete` and application isolation remains fail-closed.

### Required behavior

```text
same friendly name + wrong driver -> reject
right driver + renamed friendly name -> still classify
driver update -> retain semantic identity if possible
```

---

# 11. Priority P1 — Relay virtual microphone pair

The feature-gated bridge now includes the second independent cable; live
endpoint enumeration and ordinary-client acceptance remain open:

```text
QPWGraph Relay Sink
        |
        v
driver bounded ring
        |
        v
QPWGraph Relay Microphone
```

Do not reuse the app-routing ring.

### Flow

```text
remote peer
    -> relay decoder
    -> qpwgraph user-mode relay output
    -> WASAPI render: QPWGraph Relay Sink
    -> kernel ring
    -> QPWGraph Relay Microphone
    -> OBS / Discord / browser / DAW
```

### Acceptance

```text
[ ] OBS records received peer audio
[ ] browser microphone test receives peer audio
[ ] Discord input receives peer audio
[ ] stopping relay produces silence, not stale audio
[ ] restarting relay does not require driver restart
[ ] app-routing cable audio never leaks into relay mic
```

---

# 12. Priority P1 — Finish user-mode virtual-driver integration

Once the endpoints really exist:

- resolve semantic endpoint roles by strong driver identity;
- expose driver version;
- expose partial/incomplete install state;
- remove fake/placeholder virtual choices if endpoint is missing;
- restart only the affected worker when a virtual endpoint changes;
- never make driver availability a requirement for app startup.

### Health model

Keep:

```text
NotInstalled
Incomplete
Ready
```

Extend with:

```text
IncompatibleVersion
EndpointOpenFailed
DriverDisabled
```

if useful.

---

# 13. Priority P2 — Manual per-app rerouting/effects end to end

Once Virtual Output/Monitor exist, finish the supported driver-backed path before touching undocumented automatic routing.

---

## P2.1 Manual isolation workflow

User selects:

```text
Windows Volume Mixer
    application output
        ->
QPWGraph Virtual Output
```

qpwgraph observes that relationship.

Then:

```text
QPWGraph Virtual Monitor
    -> user-mode router
    -> effects
    -> physical destination
```

or process-loopback can remain the application-specific source if that produces cleaner per-process separation when several apps share Virtual Output.

### Important choice

Do not blindly use the monitor mix if multiple applications render to Virtual Output.

For per-application effects/routing:

```text
process loopback = application-specific PCM
virtual output    = isolation mechanism
```

For whole-virtual-output routing:

```text
virtual monitor = mixed PCM for all apps assigned there
```

Keep both concepts distinct.

---

## P2.2 Prevent accidental double routes

Before activating local per-app effects:

```text
assert session endpoint == QPWGraph Virtual Output
assert current PID matches stable selector
assert process capture is healthy
```

If not:

```text
refuse local rerender
```

Do not silently fall back to capture-only.

---

# 14. Priority P2 — Effect and route restoration

The repository implementation now restores the following as one route
transaction after manual isolation:

restore:

```text
destination
gain
effect chain
effect parameters
bypass state
enabled state
```

Use one transaction for processor creation, graph links, router links, and
source gain.

A failed effect restore must not leave half the route graph applied.

The chosen policy is to fail the entire activation and report
`EffectRestoreFailed`; saved effects are never silently bypassed. Live Windows
acceptance still needs to verify processor audio, parameter behavior, restart
reconciliation, and rollback on the supported effect host. Module-backed
effects are rejected explicitly until the Windows realtime host supports their
module path; they are never substituted with a built-in processor.

---

# 15. Priority P2 — Endpoint persistence migration

Adopt `PKEY_AudioEndpoint_StableId` without breaking old configs.

Migration:

```text
old:
destination_endpoint_id
destination_name

new:
destination.stable_id
destination.mmdevice_id
destination.name
```

On load:

1. resolve old ID;
2. read stable ID;
3. write upgraded selector on next save.

Do not require manual config conversion.

---

# 16. Priority P3 — Driver installation and lifecycle

Do this only after streaming endpoints are real.

---

## P3.1 Installer behavior

The repository lifecycle scripts now enforce the release manifest/artifact
contract, require the four provider-owned endpoint roles to be verified by the
Windows smoke probe, roll back the exact published package on failed install
verification, and verify role disappearance on uninstall. Test-signing mode
and verification bypasses are explicit switches. Live install, upgrade,
endpoint-removal, default-device, and Secure Boot evidence remain release
validation gates.

Installer:

- requests elevation only for driver operations;
- installs driver package;
- verifies expected endpoints appeared;
- records package/driver version;
- never changes Windows default input/output;
- supports rollback.

Uninstaller:

- stops qpwgraph virtual streams;
- removes driver package;
- verifies endpoints disappear;
- leaves user configuration intact unless explicitly requested.

---

## P3.2 Upgrade behavior

Test:

```text
vN installed
streams closed
upgrade -> vN+1
endpoint semantic identity retained
saved routes still resolve
```

If endpoint stable identity cannot be retained across an incompatible driver architecture change, define a migration table.

---

# 17. Priority P3 — Driver security and reliability

Required before public driver release:

```text
Driver Verifier
infverif
install/update/uninstall loops
sleep/resume
device disable/enable
audio service restart
Windows reboot
qpwgraph crash while streams active
capture client crash
render client crash
```

Kernel rules:

- no arbitrary privileged IOCTL;
- no unbounded allocation in stream path;
- no blocking user-mode dependency;
- no formatted logging from realtime callback;
- validate every length and format;
- no panic crosses FFI;
- document every unsafe wrapper;
- fail closed.

Microsoft's `windows-drivers-rs` still describes itself as early-stage and not recommended for production use, so passing Rust compilation is not a release criterion.

---

# 18. Priority P3 — HLK and signing

Release-driver gate:

```text
[ ] Driver Verifier clean
[ ] relevant HLK audio tests complete
[x] INF/package validation clean (WDK stampinf/Inf2Cat completed with no errors or warnings locally)
[ ] Microsoft signing pipeline established
[ ] Secure Boot installation verified
[ ] upgrade and uninstall verified
```

Portable user-mode ZIP remains independent.

Release tiers:

```text
Portable
    qpwgraph-rs.exe
    no driver requirement

Full
    qpwgraph-rs
    Microsoft-signed virtual audio driver
    installer
```

---

# 19. Priority P4 — Automatic per-app output routing

This remains last.

As of this plan, the supported public Core Audio APIs still do not provide a documented operation for qpwgraph to arbitrarily move another application's audio session to a selected endpoint.

Do not block the rest of Windows parity on this.

---

## P4.1 Keep the policy abstraction

Existing:

```text
AppRoutePolicy
UnsupportedAppRoutePolicy
```

Add later:

```text
VerifiedAudioPolicyConfig
```

only after obtaining a verified ABI declaration.

### Required safeguards

- explicit known IID;
- explicit interface layout;
- no guessed slots;
- no vtable probing;
- runtime interface check;
- version/build allowlist if necessary;
- opt-in config;
- manual fallback always available.

Config:

```toml
[windows]
experimental_app_routing = false
```

Default remains false until multi-build validation is strong enough.

---

# 20. Features that should NOT wait for automatic app routing

These can be completed first:

```text
single-app relay
single-app RMS
manual app isolation
per-app effects after isolation
saved app-route reconciliation after isolation
Relay Microphone
virtual cable
driver packaging
```

That is why private AudioPolicyConfig work belongs near the end.

---

# 21. Test matrix

## Process capture

```text
[x] ordinary Win32 app (deterministic helper; opt-in live smoke test passed locally)
[ ] packaged/MSIX app
[ ] browser with child processes
[x] multiple audio sessions in same process (opt-in helper live smoke test passed locally)
[x] silent process (opt-in helper live smoke test passed locally)
[x] process starts after qpwgraph (opt-in helper live smoke test passed locally)
[x] process exits during capture (opt-in live smoke test passed locally)
[x] process restarts with new PID (opt-in helper relay smoke test passed locally)
[ ] PID reused by unrelated executable
[x] 1000 activation/start/stop cycles (opt-in live process-loopback cycle test passed locally)
```

## Relay

```text
[x] normal app -> single-app relay without driver (opt-in helper smoke test passed locally)
[x] app exits -> control session remains (opt-in local host/client smoke test passed locally)
[x] app returns -> source can recover (opt-in helper smoke test passed locally)
[ ] output endpoint changes while app relay active
```

## Metering

```text
[x] native peak only (opt-in live session-meter smoke test passed locally)
[x] process RMS available (opt-in helper smoke test passed locally)
[x] process loopback unavailable -> peak fallback (opt-in live fault-injected fallback smoke test passed locally)
[x] on-demand worker closes (opt-in helper smoke test passed locally)
[x] always policy bounded (32-target cap and fallback policy unit-tested locally)
```

## Virtual driver

```text
[ ] render endpoint enumerate
[ ] render stream start/stop
[ ] capture endpoint enumerate
[ ] render -> capture deterministic tone
[ ] underflow -> silence
[ ] overflow counted
[ ] timestamp monotonic
[ ] sleep/resume
[ ] disable/enable
[ ] uninstall/reinstall
```

## Relay microphone

```text
[ ] peer -> OBS
[ ] peer -> browser
[ ] peer -> Discord
[ ] silence after disconnect
[ ] no cross-talk with app virtual cable
```

## App reroute/effects

```text
[ ] manual isolation recognized
[ ] process identity reverified
[ ] dry+processed duplication impossible
[ ] effect chain applies
[ ] effect bypass restores
[ ] destination disappears
[ ] destination returns
[ ] app restarts
```

---

# 22. New PR sequence

This sequence is intentionally different from the original parity plan.

## PR 1 — Split capture capability from route capability

- introduce read-only process-capture capability;
- keep mutable graph routing isolated-only;
- tests for ordinary vs virtualized session capabilities.

## PR 2 — Single-app relay without virtual-driver gate

- expose eligible normal render sessions;
- stable selector -> live PID activation;
- process exit/restart handling.

## PR 3 — Per-app true RMS without virtual-driver gate

- process-loopback meter source;
- on-demand/always lifetime;
- native peak fallback.

## PR 4 — Process capture manager

- shared state;
- health;
- backoff;
- generation tracking;
- consumer ownership.

## PR 5 — Stronger application identity

- package family;
- AUMID;
- selector precedence;
- packaged-app tests.

## PR 6 — Stable endpoint identity

- read `PKEY_AudioEndpoint_StableId`;
- endpoint selector migration;
- relay/destination persistence.

## PR 7 — Application-route reconciler

- persisted-rule state machine;
- retry triggers;
- degraded states.

## PR 8 — Windows diagnostics UI

- Copy Windows audio report;
- capability reason strings;
- privacy checks.

## PR 9 — ACX binding layer

- eWDK ACX bindings;
- narrow Rust wrappers;
- one driver-device creation path.

## PR 10 — First real virtual render endpoint

- `QPWGraph Virtual Output`;
- shared-mode WASAPI smoke test;
- Driver Verifier smoke.

## PR 11 — Virtual clock and packet timeline

- pure Rust clock/position core;
- ACX stream integration;
- timestamp tests.

## PR 12 — Virtual Output / Monitor cable

- capture endpoint;
- ring transport;
- tone round trip.

## PR 13 — Strong virtual endpoint identity

- driver-owned role identity;
- stable endpoint identity;
- remove friendly-name-only classification.

## PR 14 — Relay Sink / Relay Microphone

- independent cable;
- relay integration;
- OBS/browser smoke tests.

## PR 15 — Manual per-app routing + effects

- isolation detection;
- route activation;
- duplication guard;
- effect chain.

## PR 16 — Route persistence completion

- destination/effect/gain restore;
- restart recovery;
- config migration;
- transactional effect rollback with legacy ID-only fail-closed handling.

## PR 17 — Driver package lifecycle

- install;
- update;
- rollback;
- uninstall;
- release artifact.

## PR 18 — Verifier / HLK / signing

- external release gates;
- Secure Boot validation.

## PR 19 — Experimental automatic app routing

- verified ABI only;
- explicit opt-in;
- multi-build tests.

---

# 23. Definition of the next Windows milestone

Do not call the next milestone complete merely because a driver `.sys` builds.

Call **Windows parity milestone 2** complete when all of these work:

```text
[x] single-app relay from a normal application without driver (opt-in helper smoke test passed locally)
[x] true RMS for a normal application without driver (opt-in helper smoke test passed locally)
[x] stable app selectors survive restart (opt-in helper smoke test passed locally)
[ ] stable endpoint selectors survive normal endpoint churn
[x] saved application route has an explicit reconciler state (reconciler unit tests passed locally)
[ ] one real Rust ACX render endpoint enumerates and streams
[ ] virtual render/capture cable carries deterministic PCM
[ ] Relay Microphone works in an ordinary Windows capture client
[ ] per-app effects work after manual isolation (repository restore path is in place;
    live processor validation remains)
[x] portable app still works with no driver installed (opt-in backend startup smoke test passed locally)
```

Automatic private-ABI app routing is not required for this milestone.

---

# 24. Immediate coding queue

The repository-level implementation queue above is complete through the
driver bridge, package scaffolding, and transactional effect restoration. The
remaining work is live validation plus the explicitly deferred automatic
routing work:

```text
1. [x] Run the eWDK/WDK toolchain audit, compile the opt-in ACX feature, and
   stage the unsigned release package.
2. Enumerate and stream-test the app render/monitor pair on a test-signed
   Windows image.
3. Validate the independent Relay Sink/Microphone cable with ordinary clients.
4. Complete package install/update/uninstall, Verifier, HLK, signing, and
   Secure Boot evidence.
5. Validate per-app effect audio, parameters, bypass, restart recovery, and
   rollback on a manually isolated Windows session.
6. Investigate automatic AudioPolicyConfig only after all supported paths are
   validated.
```

---

# 25. Things to remove from the old plan

The next plan should no longer present these as unimplemented:

```text
ProcessLoopbackSource
single-app relay adapter
per-process RMS plumbing
process-loopback router source
stable process selector schema
AppRoutePolicy abstraction
virtual endpoint role enum
driver nested workspace
driver ring core
driver package scaffold
driver opt-in CI
Windows audio report backend
```

They are foundations that now need completion/live validation, not greenfield features.

---

# 26. External references

## Process loopback

Microsoft documents process-loopback capture using:

```text
AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK
AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS
PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE
PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE
```

Minimum documented support for the process-loopback parameter structures is Windows 10 build 20348.

- https://learn.microsoft.com/en-us/windows/win32/api/audioclientactivationparams/ne-audioclientactivationparams-process_loopback_mode
- https://learn.microsoft.com/en-us/windows/win32/api/audioclientactivationparams/ns-audioclientactivationparams-audioclient_process_loopback_params
- https://github.com/microsoft/Windows-classic-samples/tree/main/Samples/ApplicationLoopback

## Stable endpoint identity

Microsoft documents:

```text
PKEY_AudioEndpoint_StableId
```

as an opaque endpoint identifier Windows attempts to preserve across OS and driver updates.

- https://learn.microsoft.com/en-us/windows/win32/coreaudio/audio-endpoint-properties

## ACX

- https://learn.microsoft.com/en-us/windows-hardware/drivers/audio/acx-audio-class-extensions-overview
- https://learn.microsoft.com/en-us/windows-hardware/drivers/audio/acx-circuits
- https://learn.microsoft.com/en-us/windows-hardware/drivers/audio/acx-streaming
- https://learn.microsoft.com/en-us/windows-hardware/drivers/install/inf-addproperty-directive
- https://learn.microsoft.com/en-us/windows-hardware/drivers/install/creating-custom-device-properties
- https://github.com/microsoft/Windows-driver-samples/tree/main/audio/Acx

ACX is KMDF-based.

## Rust Windows drivers

- https://github.com/microsoft/windows-drivers-rs
- https://github.com/microsoft/Windows-rust-driver-samples

`windows-drivers-rs` still describes itself as early-stage and not recommended for production use, so Driver Verifier/HLK/signing remain mandatory release gates.

---

# 27. Final architecture

Target architecture after this plan:

```text
                    +--------------------------+
                    | Windows applications     |
                    +------------+-------------+
                                 |
                  normal output  |  optional virtual output
                                 |
                +----------------v----------------+
                | Core Audio / qpwgraph driver   |
                +----------------+----------------+
                                 |
              +------------------+-------------------+
              |                                      |
      read-only capture                       owned/isolated PCM
              |                                      |
      ProcessLoopbackSource                          |
              |                                      |
      +-------+---------+                    +-------v--------+
      |                 |                    | Rust router    |
      v                 v                    | effects/mix    |
  App RMS          Single-app relay          | gain/RMS       |
                                             +-------+--------+
                                                     |
                                           Physical / virtual sink


Relay receive:
peer
  -> Rust relay
  -> QPWGraph Relay Sink
  -> Rust ACX driver cable
  -> QPWGraph Relay Microphone
  -> third-party capture application
```

This delivers useful Windows improvements incrementally:

```text
process capture features first,
real virtual endpoints second,
manual owned app routing third,
private automatic routing last.
```
