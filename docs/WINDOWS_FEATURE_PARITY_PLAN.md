# Windows Feature Parity Implementation Plan for `qpwgraph-rs`

Repository: https://github.com/nglmercer/qpwgraph-rs

## Goal

Bring the Windows backend as close as technically possible to Linux/PipeWire feature parity while keeping the existing Rust user-mode router as the central PCM engine.

The Windows implementation should add:

- A qpwgraph-owned virtual audio device implemented as a Windows audio driver written in Rust.
- A virtual render/capture path that other Windows applications can select.
- A real **Relay Microphone** capture endpoint for received relay audio.
- Per-process loopback capture for a single application.
- Per-application relay source selection.
- Per-application true RMS metering.
- Per-application effects by moving/capturing application PCM through the qpwgraph router.
- Best-effort per-application output-device routing.
- Windows packaging, driver installation, signing, CI, diagnostics, and recovery.

The existing Core Audio/WASAPI endpoint/session graph, WinMM MIDI implementation, user-mode router, effects engine, patchbay, relay engine, and Slint UI should be reused rather than replaced.

---

## 1. Current Windows Baseline

The repository already has a large Windows implementation.

Existing Windows functionality includes:

- Core Audio render/capture endpoint enumeration.
- Active application audio session enumeration.
- Endpoint and session volume control.
- Endpoint and session mute control.
- Native peak metering where Core Audio exposes it.
- Event-driven endpoint/session notifications.
- Real device-to-device WASAPI routing through the Rust user-mode router.
- Render-loopback source support.
- Fan-out, fan-in, mixing, resampling, gain, and channel conversion.
- Effects on routes qpwgraph owns.
- True RMS metering on routes qpwgraph owns.
- Patchbay persistence for mutable qpwgraph-owned routes.
- WinMM MIDI enumeration and mutable routing.
- MIDI fan-out and fan-in.
- Relay input/output through physical WASAPI endpoints.
- Windows tray integration.
- Portable Windows ZIP releases.
- Windows-native CI builds.

Relevant repository areas:

```text
crates/pw-graph-backend/src/windows/
    callbacks.rs
    driver.rs
    effects.rs
    identity.rs
    routing.rs
    worker.rs

crates/pw-graph-backend/src/router/
crates/pw-graph-backend/src/windows_relay.rs
crates/pw-graph-backend/src/windows_midi.rs
crates/pw-graph-effects/
crates/pw-graph-relay/
crates/pw-graph-app-core/
crates/pw-graph-app/
```

The existing router should remain the only application-level PCM graph engine.

---

## 2. Windows Feature Status

### 2.1 Virtual qpwgraph-owned Windows audio endpoints

Status: **Missing / driver required**

Current WASAPI routing can move audio only between endpoints that already exist.

qpwgraph cannot currently expose:

- a render endpoint that arbitrary applications can select as an output;
- a capture endpoint that arbitrary applications can select as a microphone;
- a virtual Relay Microphone.

This is the most important structural Windows gap.

### 2.2 Single-application capture

Status: **Implemented for manually virtualized sessions**

Use Windows process-loopback capture:

```text
ActivateAudioInterfaceAsync
    + VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK
    + AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK
```

This API can capture a target process tree independently of the physical output endpoint.

Minimum documented OS support is Windows 10 build 20348.

### 2.3 Single-application relay

Status: **Implemented for manually virtualized sessions**

Once process-loopback capture exists, the selected process PCM can feed:

```text
process loopback
    -> bounded qpwgraph PCM hand-off
    -> relay encoder
    -> peer
```

### 2.4 Per-application true RMS metering

Status: **Implemented on qpwgraph-owned process routes**

Core Audio session metering is peak-only.

Once qpwgraph captures actual process PCM, compute:

- peak;
- RMS;
- clipping;
- route diagnostics;

with the same router meter implementation already used for routed endpoint audio.

### 2.5 Per-application effects

Status: **Implemented on qpwgraph-owned process routes**

Effects require qpwgraph to own the PCM.

Once application PCM is captured into the router, allow:

```text
Application
    -> ProcessLoopbackSource
    -> Effect A
    -> Effect B
    -> Destination
```

This must be combined with a strategy that prevents the application's original stream from also reaching its old endpoint.

### 2.6 Per-application output routing

Status: **Blocked / undocumented Windows ABI**

Windows Settings supports persisted per-application output selection, but the API used by Windows for this is not a supported public Core Audio API.

The repository already documents the internal `Windows.Media.Internal.AudioPolicyConfig` investigation.

Implementation should therefore be:

- isolated;
- runtime-gated;
- never based on vtable probing;
- never required for normal app startup;
- allowed to fail safely;
- backed by a manual Windows Settings fallback.

---

# 3. Target Architecture

## 3.1 Keep kernel mode minimal

Do **not** move mixing, effects, relay, resampling, routing policy, patchbay logic, or UI policy into the driver.

The kernel component should expose virtual audio endpoints and transport PCM.

The existing Rust router remains responsible for:

- mixing;
- fan-out/fan-in;
- sample-rate conversion;
- channel mapping;
- software gain;
- effects;
- true meters;
- route diagnostics;
- relay integration.

Target:

```text
+-------------------------------------------------------+
| qpwgraph-rs user mode                                |
|                                                       |
|  Core Audio observation                              |
|  Process loopback capture                            |
|  WinMM MIDI                                          |
|                                                       |
|  +-----------------------------------------------+    |
|  | Existing Rust Router                          |    |
|  | mixing / effects / RMS / resampling / gain    |    |
|  +-----------------------------------------------+    |
|                 |                    |                |
|                 |                    |                |
|        Physical WASAPI         Virtual WASAPI         |
+-----------------|--------------------|----------------+
                  |                    |
                  |             +------v----------------+
                  |             | Rust Windows Driver    |
                  |             | virtual audio endpoints|
                  |             +------------------------+
                  |
              Speakers / Mics
```

---

# 4. Rust Windows Audio Driver

## 4.1 Driver requirement

The driver runtime code should be written in Rust.

Acceptable non-Rust files:

- INF/INX driver-install metadata;
- CAT output;
- generated bindings;
- build scripts;
- Microsoft SDK/WDK headers consumed by bindgen.

Avoid adding a production C/C++ driver implementation.

## 4.2 Recommended framework: ACX-first

Start with **Audio Class Extensions (ACX)** rather than directly cloning SysVAD.

Reasons:

- ACX is Microsoft's modern WDF-based Windows audio-driver model.
- ACX is C-style/WDF-handle oriented and maps better to Rust FFI than the older C++-heavy PortCls SysVAD architecture.
- ACX supports WaveRT streaming, which is sufficient for this virtual device.
- Microsoft ships current ACX sample drivers.

Fallback only if the Stage-0 prototype proves ACX bindings unusable:

- implement WDM/PortCls in Rust using generated WDK bindings;
- use SysVAD only as behavioral/reference material;
- still keep the production driver code in Rust.

## 4.3 Rust WDK tooling

Use Microsoft's Rust driver ecosystem as the starting point:

```text
microsoft/windows-drivers-rs
microsoft/Windows-rust-driver-samples
```

Expected driver properties:

```rust
#![no_std]
#![no_main]
panic = "abort"
crate-type = ["cdylib"]
```

Likely dependencies:

```text
wdk
wdk-sys
wdk-build
wdk-alloc
wdk-panic
```

All raw ACX/WDF calls should be isolated behind a small unsafe wrapper layer.

Important: Microsoft's Rust driver project currently describes itself as early-stage and not recommended for production use. Treat this as a release risk and require Driver Verifier + HLK qualification before enabling the driver by default.

---

# 5. Proposed Driver Endpoint Model

Use separate virtual paths so unrelated scenarios never accidentally mix.

## 5.1 App-routing virtual path

Expose:

```text
QPWGraph Virtual Output      [render endpoint]
QPWGraph Virtual Monitor     [capture endpoint]
```

Semantics:

```text
Windows application
    -> QPWGraph Virtual Output
    -> driver ring
    -> QPWGraph Virtual Monitor
    -> qpwgraph WASAPI capture
    -> router
    -> selected physical output/effects/relay
```

This gives qpwgraph ownership of application PCM after the app is moved to the virtual device.

## 5.2 Relay microphone virtual path

Expose:

```text
QPWGraph Relay Sink          [render endpoint]
QPWGraph Relay Microphone    [capture endpoint]
```

Semantics:

```text
network peer
    -> qpwgraph relay receive
    -> router
    -> WASAPI render into QPWGraph Relay Sink
    -> driver ring
    -> QPWGraph Relay Microphone
    -> Discord / OBS / DAW / browser / etc.
```

Benefits of a second pair:

- relay audio is not mixed with application-routing audio;
- users can independently select the relay mic;
- shutdown/restart behavior is simpler;
- patchbay semantics stay understandable.

## 5.3 Initial audio formats

Start conservative:

- 48 kHz;
- stereo;
- 32-bit float if ACX path supports it cleanly;
- PCM16 fallback.

Then add format negotiation for:

- 44.1 kHz;
- 48 kHz;
- mono;
- stereo;
- PCM16;
- float32.

The user-mode router already owns general resampling/channel conversion.

Do not add complex kernel-side resampling.

---

# 6. Driver Repository Layout

Keep driver build requirements separate from the normal portable workspace.

Recommended layout:

```text
drivers/
  windows-audio/
    Cargo.toml
    Cargo.lock
    .cargo/
      config.toml

    driver/
      Cargo.toml
      build.rs
      src/
        lib.rs
        driver.rs
        device.rs
        acx.rs
        circuit.rs
        stream.rs
        ring.rs
        formats.rs
        telemetry.rs
        ffi.rs

    package/
      qpwgraph-audio.inx
      qpwgraph-audio-extension.inx
      README.md

    xtask/
      Cargo.toml
      src/main.rs

    tests/
      smoke/
```

Prefer a nested Cargo workspace.

Why:

- WDK metadata does not affect the normal Linux/Android workspace.
- `cargo test --workspace` on Linux should not attempt to build a kernel driver.
- the driver can pin its WDK/Rust-driver dependencies independently.
- Windows driver packaging can run in its own CI job.

---

# 7. Stage 0 — Driver Feasibility Spike

Do this before building any higher-level feature.

## Deliverables

- Build a Rust KMDF/ACX driver on a current eWDK.
- Install it on a test Windows VM/machine.
- Expose one virtual render endpoint.
- Make it visible in:
  - Device Manager;
  - Windows Sound Settings;
  - `IMMDeviceEnumerator`.
- Open the endpoint with WASAPI.
- Start/stop streams repeatedly.
- Uninstall without leaving stale endpoints.
- Survive Driver Verifier.

## ACX binding test

Prove Rust can express:

- driver initialization;
- WDF device creation;
- ACX device/circuit creation;
- pins;
- stream creation;
- WaveRT packet callbacks;
- format negotiation;
- cleanup/power transitions.

If ACX headers are not generated correctly by `windows-drivers-rs`:

1. add a local bindgen build step for the required ACX headers;
2. wrap generated bindings in `driver/src/ffi.rs`;
3. re-implement problematic macros/constants in Rust where necessary.

Do not switch to C++ merely because bindgen needs a small amount of manual work.

## Exit criteria

Stage 0 passes only when:

```text
[ ] Rust driver loads
[ ] endpoint appears
[ ] WASAPI can open it
[ ] audio stream start/stop is stable
[ ] device disable/enable is stable
[ ] driver unload is clean
[ ] Driver Verifier shows no immediate violations
```

---

# 8. Stage 1 — Virtual Render/Capture Loop

Implement the minimum useful virtual cable.

## Tasks

- Add render circuit.
- Add capture circuit.
- Add bounded lock-free/ring transport between them.
- Track:
  - write position;
  - read position;
  - discontinuities;
  - underflow;
  - overflow;
  - active stream count.
- Fill capture with silence on render starvation.
- Drop oldest/newest frames according to one documented policy on overflow.
- Never allocate in the realtime packet path.
- Never format strings in the realtime packet path.
- Never hold an unbounded mutex in stream callbacks.
- Define clean device-removal behavior.

## Tests

```text
render sine -> capture -> verify frequency/amplitude
render silence -> capture silence
render disconnect -> capture recovers
capture disconnect -> render continues safely
format reopen loop x1000
sleep/resume
device disable/enable
app crash while stream open
```

---

# 9. Stage 2 — Expose Both Virtual Paths

Add the four user-visible endpoints:

```text
QPWGraph Virtual Output
QPWGraph Virtual Monitor
QPWGraph Relay Sink
QPWGraph Relay Microphone
```

Requirements:

- stable endpoint identity across reboot;
- predictable friendly names;
- no duplicate devices after driver update;
- endpoint GUID/version migration plan;
- clean uninstall;
- no default-device takeover during installation.

The installer must never silently make qpwgraph's virtual device the Windows default.

---

# 10. Stage 3 — User-Mode Driver Integration

The qpwgraph app should treat the driver endpoints as normal Core Audio endpoints wherever possible.

Add a small identity layer:

```text
crates/pw-graph-backend/src/windows/virtual_device.rs
```

Responsibilities:

- detect qpwgraph driver endpoints;
- classify their roles;
- avoid showing implementation-only endpoints in confusing contexts;
- map stable IDs to semantic roles;
- expose driver installed/version/health state.

Possible model:

```rust
enum QpwVirtualEndpointRole {
    AppRender,
    AppMonitor,
    RelayRender,
    RelayCapture,
}
```

Do not hard-code raw endpoint IDs in configuration files.

Persist semantic role + driver endpoint instance identity.

---

# 11. Stage 4 — Relay Microphone

Current direct Windows receiver mode should remain.

Add a second receiver destination mode:

```text
Direct output
Virtual microphone
```

## Direct output

Existing behavior:

```text
peer -> selected physical eRender endpoint
```

## Virtual microphone

New behavior:

```text
peer
 -> relay decoder
 -> router
 -> QPWGraph Relay Sink
 -> QPWGraph Relay Microphone
 -> third-party application
```

UI:

```text
Relay receive target:
( ) Speakers / selected output
( ) QPWGraph Relay Microphone
```

When the driver is absent:

- hide/disable virtual-microphone mode;
- explain that direct output still works;
- never fail relay startup globally.

Acceptance:

```text
[ ] OBS can select QPWGraph Relay Microphone
[ ] Discord/browser can select it
[ ] peer audio reaches selected application
[ ] direct receiver still works without driver
[ ] driver removal while active fails gracefully
```

---

# 12. Stage 5 — Safe Process-Loopback Capture

Implement Microsoft's ApplicationLoopback pattern directly.

Add:

```text
crates/pw-graph-backend/src/windows/process_loopback.rs
```

## Critical lifetime requirement

The previous attempt documented by the repository crashed with `STATUS_HEAP_CORRUPTION`.

Avoid temporary activation-data lifetimes.

The following objects must remain alive until `ActivateCompleted` finishes:

- `AUDIOCLIENT_ACTIVATION_PARAMS`;
- the `VT_BLOB` data;
- the `PROPVARIANT`;
- the async operation;
- the completion handler state.

Use an owning state object.

Conceptually:

```rust
struct ProcessLoopbackActivation {
    params: Box<AUDIOCLIENT_ACTIVATION_PARAMS>,
    propvariant: OwnedPropVariantBlob,
    operation: Option<IActivateAudioInterfaceAsyncOperation>,
    completion: Arc<ActivationState>,
}
```

Do not construct a `PROPVARIANT` whose blob points to stack memory that returns before asynchronous activation completes.

## API

```rust
pub struct ProcessLoopbackSource {
    pid: u32,
    include_process_tree: bool,
    ...
}
```

Methods:

```text
open(pid)
start()
stop()
read()
recover()
```

Support:

- include target process tree;
- optionally exclude target process tree for future use.

## OS compatibility

Do not make app startup depend on a version check.

Capability detection should be operational:

1. try supported activation;
2. handle returned HRESULT;
3. cache capability;
4. expose a clear UI reason when unavailable.

Full single-app capture is available only where the OS supports process loopback.

The repository implementation is fail-closed and currently exposes the source
only for a live render session already assigned to `QPWGraph Virtual Output`.
`ProcessLoopbackSource` feeds a bounded router source, reports loss when the
process exits, and carries a generation so a reused PID cannot inherit the old
route. The relay reuses the same activation path and resolves a stable
executable selector to the current PID at start time; no PID is persisted.

---

# 13. Stage 6 — Integrate Process PCM Into Router

Represent process capture as a real router source.

Possible new source key:

```rust
enum WindowsPcmSourceKey {
    Endpoint(...),
    RenderLoopback(...),
    Process { pid: u32, generation: u64 },
}
```

Requirements:

- process exit invalidates source cleanly;
- PID reuse cannot attach a saved route to the wrong process;
- pair PID with process executable identity/session instance where possible;
- capture restart increments generation;
- no stale route survives into a different process with the same numeric PID.

The graph node remains the existing application session node.

The PCM source is an internal capability associated with that node.

---

# 14. Stage 7 — Single-Application Relay

Once process PCM is routable:

```text
Application session
    -> ProcessLoopbackSource
    -> relay
```

This path is now wired for the manually virtualized sessions described above.
The Windows relay advertises `application:<stable-selector>` choices only when
Core Audio reports the live session on QPWGraph Virtual Output. Automatic
per-application endpoint reassignment remains behind the unsupported-policy
fallback and is not required for startup.

UI additions:

- application sessions become eligible relay sources when process loopback is available;
- current endpoint/monitor choices remain;
- show a capability badge when process-loopback capture is unavailable on this OS.

Persistence:

Do not persist only a PID.

Persist a process selector such as:

```text
exe identity
package identity if available
session identity hints
friendly display name
```

Resolve to a live PID each run.

---

# 15. Stage 8 — Per-Application RMS Metering

For a session with active process capture:

```text
PCM
 -> existing router meter
 -> peak + RMS
```

Rules:

- native session meter remains the fallback;
- process capture replaces peak-only readings only while active and healthy;
- never claim RMS support unless PCM is actually available.

Capability transition:

```text
Before capture:
    meter_peak = true/false from Core Audio
    meter_rms  = false

During process PCM capture:
    meter_peak = true
    meter_rms  = true
```

Use the same meter policy:

- disabled;
- on-demand;
- always.

Do not permanently run process loopback merely to make cards prettier unless policy requests it.

---

# 16. Stage 9 — Per-Application Effects

Per-app effects require two independent pieces:

1. isolated process PCM;
2. preventing the original application route from also being heard.

Preferred workflow:

```text
App
 -> QPWGraph Virtual Output
 -> process loopback / virtual capture
 -> qpwgraph router
 -> effects
 -> selected physical endpoint
```

Implementation sequence:

- create a qpwgraph-owned app route object;
- activate process capture;
- move app output to `QPWGraph Virtual Output` when automatic app routing is available;
- otherwise instruct the user to select `QPWGraph Virtual Output` in Windows Sound settings;
- route isolated process PCM through effect nodes;
- render only the processed stream to destination.

Never silently create duplicate audio.

If qpwgraph cannot confirm that the original path is no longer audible, per-app effect activation should fail with an actionable explanation rather than play dry + processed audio simultaneously.

---

# 17. Stage 10 — Per-Application Output Routing

## 17.1 Public API reality

As of this plan, there is no documented public Win32 Core Audio API that lets one process arbitrarily move another application's session between endpoints.

Windows itself exposes the feature in Settings.

Known implementations use an undocumented WinRT audio policy interface.

Therefore this feature must be isolated.

## 17.2 Add an abstraction

```text
crates/pw-graph-backend/src/windows/app_route_policy.rs
```

API:

```rust
trait AppRoutePolicy {
    fn support(&self) -> AppRoutePolicySupport;
    fn get_persisted_endpoint(&self, process: ProcessIdentity) -> Result<...>;
    fn set_persisted_endpoint(
        &self,
        process: ProcessIdentity,
        flow: AudioFlow,
        role: AudioRole,
        endpoint: Option<&str>,
    ) -> Result<...>;
}
```

Implementations:

```text
UnsupportedAppRoutePolicy
WinRtAudioPolicyConfig
```

## 17.3 Undocumented WinRT implementation rules

Never probe unknown vtable slots.

Use explicit known interface declarations only.

At runtime:

1. activate `Windows.Media.Internal.AudioPolicyConfig`;
2. query only known IID(s);
3. if IID is absent, report unsupported;
4. call only methods from a verified declaration;
5. validate on every supported Windows build family;
6. never assume Windows 10 and Windows 11 share an ABI;
7. isolate all unsafe calls in one module;
8. add crash-free conformance tests before enabling.

Add a feature/config safety switch:

```text
experimental_app_routing = false/true
```

Until validated across supported OS versions, ship it opt-in.

## 17.4 Manual fallback

When automatic routing is unavailable:

```text
"Set this app's output to QPWGraph Virtual Output in
Settings > System > Sound > Volume mixer."
```

After the user changes it, qpwgraph detects the new session relationship and continues automatically.

This keeps the supported route based on documented Windows behavior even if the internal policy ABI changes.

---

# 18. Stage 11 — Graph Semantics

The Windows graph must clearly distinguish:

```text
Observed system relationship
qpwgraph-owned PCM route
qpwgraph-owned virtualized app route
WinMM MIDI route
```

Suggested link metadata:

```rust
enum LinkOwnership {
    Observed,
    QpwRouter,
    QpwVirtualAppRoute,
    NativeMidi,
}
```

Rules:

- observed Core Audio session links remain immutable;
- virtualized app routes are mutable because qpwgraph owns their destination and PCM path;
- effects may be inserted only on owned PCM routes;
- patchbay persistence includes only owned/mutable links.

---

# 19. Stage 12 — Patchbay Persistence

A persisted application route must never rely on transient PID.

Persist:

```text
application selector
destination stable endpoint selector
virtualization required flag
effect chain identity
volume/gain
enabled state
```

Suggested selector structure:

```rust
struct WindowsApplicationSelector {
    executable_path_hash: Option<...>,
    executable_name: Option<String>,
    package_family_name: Option<String>,
    app_user_model_id: Option<String>,
    display_name: Option<String>,
}
```

Activation algorithm:

1. find matching live session;
2. resolve current PID;
3. verify process identity;
4. activate process capture;
5. move app to virtual sink if supported/required;
6. construct router route;
7. restore effect chain;
8. mark route degraded if any step fails.

Never apply a saved route to an unrelated process just because it reused a PID.

---

# 20. Stage 13 — Device and Process Recovery

Handle:

- app process exits;
- app restarts;
- endpoint disappears;
- Bluetooth endpoint profile changes;
- default output changes;
- driver is disabled;
- driver is upgraded;
- audio service restarts;
- system sleep/resume;
- sample-rate changes;
- WASAPI device invalidation;
- relay peer disconnect;
- qpwgraph crashes.

Expected behavior:

```text
Driver endpoints:
    remain valid if qpwgraph process exits.

qpwgraph-owned process route:
    goes inactive when process disappears.

Persisted rule:
    can reactivate when the application returns.

Physical destination loss:
    route enters degraded state and follows existing recovery policy.

Driver removal:
    app continues without virtual-driver-only features.
```

---

# 21. Stage 14 — Diagnostics

Expose diagnostics for each owned application route:

- process identity;
- PID/generation;
- process-loopback state;
- virtual endpoint state;
- physical destination;
- frames captured;
- frames rendered;
- source starvation;
- sink overrun;
- discontinuities;
- restart count;
- resampler ratio;
- drift ppm;
- effect processing time;
- last HRESULT;
- last router fault.

Add a Windows diagnostics panel/export:

```text
Help -> Diagnostics -> Copy Windows audio report
```

The report must not include raw audio.

---

# 22. Stage 15 — Security Requirements

The kernel driver is a new privileged attack surface.

Rules:

- no arbitrary kernel memory reads/writes;
- no general-purpose privileged IOCTL;
- prefer standard audio streams instead of a custom kernel control channel;
- validate every buffer length;
- validate every format;
- use bounded arithmetic;
- no unchecked user pointers;
- no panics across FFI;
- panic abort only;
- minimize `unsafe`;
- document every unsafe block;
- enable Control Flow Guard/WDK hardening where supported;
- run Driver Verifier continuously during development;
- use Microsoft's driver security checklist before signing.

Keeping all communication on normal render/capture endpoints is preferred because it avoids inventing a privileged qpwgraph-specific control protocol.

---

# 23. Stage 16 — Driver Testing

## Unit tests

Pure Rust units for:

- ring wraparound;
- overflow;
- underflow;
- format calculations;
- packet positions;
- frame counters;
- state transitions;
- endpoint identity;
- configuration parsing.

## VM/test-machine tests

Automate:

```text
install
enumerate
stream
disable
enable
sleep/resume
upgrade
uninstall
reinstall
```

## Driver Verifier

Run with checks relevant to:

- IRQL;
- pool;
- I/O verification;
- deadlocks;
- DMA if applicable;
- DDI compliance;
- security.

## HLK

Run current Windows Hardware Lab Kit audio tests.

Track every expected failure and do not waive failures casually.

The goal is a Microsoft-signed production package, not only a test-signed `.sys`.

---

# 24. Stage 17 — Process-Loopback Tests

Test against:

- browser media;
- Firefox/Chrome/Edge;
- VLC;
- games;
- DAWs where practical;
- packaged applications;
- multiple processes sharing one app identity;
- process tree children;
- applications with several audio sessions.

Cases:

```text
[ ] target process produces audio
[ ] target process silent
[ ] target exits during capture
[ ] child process starts after capture begins
[ ] default endpoint changes
[ ] target endpoint disappears
[ ] capture stop/start loop
[ ] 1000 activation cycles
[ ] no heap corruption under Application Verifier
```

Also test known edge cases where process-loopback APIs may return silence for protected/special audio paths.

Treat such OS/application restrictions as capabilities, not crashes.

---

# 25. Stage 18 — Integration Tests

Add Windows-only opt-in tests similar to existing live-audio tests.

Environment flags:

```text
PW_GRAPH_TEST_PROCESS_LOOPBACK=1
PW_GRAPH_TEST_VIRTUAL_DRIVER=1
PW_GRAPH_TEST_APP_ROUTING=1
PW_GRAPH_TEST_RELAY_MIC=1
```

Automated test helper app:

```text
crates/windows-audio-test-tone/
```

The helper should:

- render deterministic audio;
- expose its PID;
- support start/stop;
- optionally open multiple sessions.

Assertions:

- process capture receives expected tone;
- RMS is approximately expected;
- effect changes measurable output;
- virtual endpoint carries the tone;
- relay microphone carries received test PCM.

---

# 26. Stage 19 — CI

Keep current Windows application CI.

Add a separate driver job.

Example conceptual matrix:

```text
windows-user-mode
    cargo test
    clippy
    feature matrix
    release build

windows-driver-build
    install/use eWDK environment
    cargo driver build
    INF validation
    package generation
    static analysis

windows-driver-tests
    self-hosted or dedicated VM
    install test-signed driver
    smoke audio
    verifier subset
```

Do not attempt to install a kernel driver on ordinary GitHub-hosted runners unless the environment explicitly supports it.

CI artifacts for development:

```text
qpwgraph-audio-test-signed.zip
```

Production release artifacts must come from the real signing pipeline.

---

# 27. Stage 20 — Packaging

Current Windows ZIP is portable and should remain available.

Add two distribution tiers.

## Tier A — Portable

```text
qpwgraph-rs-X.Y.Z-x86_64-pc-windows-msvc.zip
```

Contains:

- user-mode application;
- README;
- license.

Features requiring the virtual driver are disabled.

## Tier B — Full Windows package

Contains:

- qpwgraph-rs executable;
- signed driver `.sys`;
- `.inf`;
- signed `.cat`;
- installer/uninstaller;
- driver version manifest.

Installer responsibilities:

- require elevation only for driver installation;
- install/update driver;
- preserve user configuration;
- never change default audio devices automatically;
- support clean rollback;
- remove endpoints on uninstall.

---

# 28. Stage 21 — Driver Signing

For public Windows 10/11 deployment, the kernel driver must be Microsoft-signed through the Windows Hardware Dev Center.

Release prerequisites:

```text
[ ] organization registered in Hardware Dev Center
[ ] EV certificate associated with account
[ ] production driver package signed for submission
[ ] HLK results or chosen Microsoft signing path completed
[ ] returned Microsoft-signed catalog/package verified
[ ] Secure Boot installation tested
```

Use test signing only on development machines.

Do not ask end users to disable Secure Boot for production.

---

# 29. Stage 22 — UI/UX Changes

## Node capability badges

Possible badges:

```text
Peak
RMS
Routable
Virtualized
Process capture
Driver required
Experimental routing
```

## Application node actions

When supported:

```text
Route application...
Insert effect...
Use as relay source
Enable true RMS meter
```

When unsupported, explain exactly why:

```text
"Single-application capture requires a newer Windows audio API."

"Automatic per-app routing is unavailable on this Windows build.
Select QPWGraph Virtual Output in Windows Volume Mixer instead."

"QPWGraph Relay Microphone requires the optional virtual audio driver."
```

Avoid generic `Unsupported` messages.

---

# 30. Stage 23 — Configuration

Add a Windows section.

Example:

```toml
[windows]
enable_process_loopback = true
experimental_app_routing = false
prefer_virtual_app_routes = true

[windows.virtual_audio]
enabled = true

[windows.relay]
receive_target = "direct"
```

Driver detection should override impossible configuration safely.

For example, `receive_target = "virtual-microphone"` must fall back with a visible warning if the driver is absent.

---

# 31. Stage 24 — Documentation

Update:

```text
docs/features.md
docs/platform-parity.md
docs/audio-router.md
docs/audio-relay.md
docs/architecture.md
docs/building.md
docs/packaging.md
```

Add:

```text
docs/windows-virtual-audio-driver.md
docs/windows-process-loopback.md
docs/windows-app-routing.md
docs/windows-driver-development.md
```

The platform-parity table should distinguish:

```text
Windows without optional driver
Windows with optional qpwgraph audio driver
```

---

# 32. Recommended Implementation Order

Do not start with undocumented app-routing APIs.

Order:

```text
1. Rust ACX feasibility spike
2. one virtual render endpoint
3. virtual render/capture loop
4. two independent virtual pairs
5. user-mode endpoint classification
6. Relay Microphone
7. safe process-loopback implementation
8. process source -> router
9. single-app relay
10. per-app RMS
11. manual virtualized app routing
12. per-app effects
13. patchbay persistence
14. undocumented automatic app routing
15. installer/signing/HLK hardening
```

This order delivers useful features even if the undocumented routing API never becomes safe enough to enable by default.

---

# 33. Suggested PR Breakdown

## PR 1 — Windows capability model

- formalize missing Windows capabilities;
- add semantic virtual-endpoint roles;
- no behavior changes.

## PR 2 — Process loopback safe prototype

- lifetime-safe async activation;
- test helper;
- no UI.

## PR 3 — Process loopback router source

- integrate with router;
- metrics;
- stop/restart.

## PR 4 — Per-process RMS

- expose real RMS when process capture active.

## PR 5 — Single-app relay

- selected application -> relay.

## PR 6 — Driver workspace bootstrap

- Rust WDK build;
- test-signed package;
- no audio endpoint yet.

## PR 7 — ACX virtual render endpoint

- one endpoint;
- enumerate/open tests.

## PR 8 — Virtual cable A

- render -> capture transport.

## PR 9 — Relay virtual pair

- Relay Sink + Relay Microphone.

## PR 10 — qpwgraph driver integration

- detect/classify endpoints;
- driver health/version.

## PR 11 — Relay Microphone UI

- receive target selector.

## PR 12 — Manual app virtualization

- recognize app sent to virtual sink;
- app route owns PCM.

## PR 13 — Per-app effects

- effect insertion on virtualized process route.

## PR 14 — Patchbay restore

- process identity selector;
- virtual route restore.

## PR 15 — Automatic app policy routing

- isolated undocumented ABI;
- opt-in.

## PR 16 — Driver packaging

- INF/CAT;
- installer;
- uninstall/upgrade.

## PR 17 — HLK/security/release hardening

- verifier;
- signing;
- release gates.

---

# 34. Definition of Windows "Full Parity"

Windows cannot become internally identical to PipeWire.

For this project, call Windows full parity complete when the following user-visible scenarios work.

## Graph

```text
[ ] physical endpoints visible
[ ] application sessions visible
[ ] MIDI devices visible
[ ] qpwgraph virtual endpoints visible
```

## Routing

```text
[ ] microphone -> speakers
[ ] speaker monitor -> another output
[ ] application -> qpwgraph virtual output -> chosen destination
[ ] routes disconnect cleanly
[ ] routes restore after restart
```

## Controls

```text
[ ] endpoint volume/mute
[ ] session volume/mute
[ ] qpwgraph route gain up to configured max
```

## Metering

```text
[ ] endpoint peak
[ ] endpoint RMS when routed
[ ] session peak fallback
[ ] per-app true RMS when process PCM is captured
```

## Effects

```text
[ ] endpoint route effects
[ ] per-app effects on virtualized app routes
[ ] effect persistence
[ ] bypass/restoration
```

## Relay

```text
[ ] physical input source
[ ] output-monitor source
[ ] single-application source
[ ] direct receiver output
[ ] QPWGraph Relay Microphone receiver output
```

## MIDI

```text
[ ] WinMM enumeration
[ ] connect/disconnect
[ ] fan-out
[ ] fan-in
[ ] persistence
```

## Reliability

```text
[ ] sleep/resume
[ ] endpoint hotplug
[ ] process exit/restart
[ ] driver update
[ ] driver uninstall
[ ] audio service restart
[ ] qpwgraph crash/restart
```

## Release

```text
[ ] normal portable app works without driver
[ ] full installer installs signed driver
[ ] Secure Boot remains enabled
[ ] driver is Microsoft-signed
[ ] Driver Verifier clean
[ ] HLK plan completed
```

---

# 35. Explicit Non-Goals

Do not expand this parity project into unrelated Windows audio work.

Out of scope unless separately requested:

- ASIO host/device support;
- replacing WinMM with full MIDI 2.0;
- DRM/protected-content interception;
- capture of protected communication audio that Windows intentionally blocks;
- kernel-side effects;
- kernel-side network relay;
- global Windows audio-engine replacement;
- unsupported registry hacks for routing;
- arbitrary vtable probing.

---

# 36. Main Risks

## Risk 1 — Rust driver ecosystem maturity

Microsoft's Rust WDK support is still early-stage.

Mitigation:

- narrow driver scope;
- ACX-first proof;
- isolate unsafe FFI;
- no custom privileged control protocol;
- Driver Verifier;
- HLK;
- extensive test-machine coverage.

## Risk 2 — ACX bindgen compatibility

Some WDK macros/types may not bind cleanly.

Mitigation:

- generate only required headers;
- hand-wrap macros/constants;
- keep `ffi.rs` tiny;
- fallback to Rust WDM/PortCls only after measured ACX failure.

## Risk 3 — Undocumented app routing ABI

Windows can change it.

Mitigation:

- never require it for startup;
- runtime IID checks;
- opt-in initially;
- manual Settings fallback;
- no vtable guessing.

## Risk 4 — Process-loopback lifetime bugs

The repository already observed heap corruption in a previous attempt.

Mitigation:

- mirror Microsoft's ApplicationLoopback object lifetime exactly;
- make activation blob owned for the complete async operation;
- stress 1000+ activation cycles;
- run Application Verifier;
- fail closed.

## Risk 5 — Duplicate application audio

Capturing and re-rendering an application without moving it can produce dry + processed duplicate audio.

Mitigation:

- require virtualized destination before enabling per-app effects/reroute;
- detect ownership state;
- abort route creation if qpwgraph cannot prove the original path is handled.

## Risk 6 — Driver signing cost/process

A test-signed driver is not a production release.

Mitigation:

- start Hardware Dev Center/EV planning before final driver milestone;
- keep portable driverless distribution working independently.

---

# 37. First Concrete Coding Tasks

Start here.

```text
[x] Add windows/process_loopback.rs
[x] Port Microsoft's ApplicationLoopback activation lifetime exactly
[x] Create deterministic Windows test-tone helper
[x] Add process-loopback opt-in integration test
[x] Feed ProcessLoopbackSource into RouterCore
[x] Expose per-process RMS on qpwgraph-owned routes
[x] Add single-app relay source

[x] Create drivers/windows-audio nested Rust workspace
[ ] Build minimal Rust KMDF driver using windows-drivers-rs
[ ] Generate/import required ACX bindings
[ ] Expose one virtual render endpoint
[ ] Pass WASAPI open/close smoke test
[ ] Implement virtual render->capture ring
[ ] Add Relay Sink/Relay Microphone pair
[x] Detect semantic endpoints in qpwgraph-rs

[x] Add manual virtualized application route
[x] Add per-app effects on manually virtualized routes
[ ] Add application route persistence
[x] Add isolated AppRoutePolicy abstraction
[ ] Implement experimental AudioPolicyConfig backend only after declarations are verified

[x] Add fail-closed driver package build scaffold
[x] Add package/INF marker checks (full `infverif` validation remains WDK-gated)
[x] Add installer/uninstaller guard rails
[ ] Add Driver Verifier test plan
[ ] Add HLK/signing release workflow
```

---

# 38. Research References

## qpwgraph-rs

- https://github.com/nglmercer/qpwgraph-rs
- https://github.com/nglmercer/qpwgraph-rs/blob/main/docs/platform-parity.md
- https://github.com/nglmercer/qpwgraph-rs/blob/main/docs/audio-router.md
- https://github.com/nglmercer/qpwgraph-rs/blob/main/docs/audio-relay.md
- https://github.com/nglmercer/qpwgraph-rs/blob/main/docs/architecture.md
- https://github.com/nglmercer/qpwgraph-rs/blob/main/docs/building.md
- https://github.com/nglmercer/qpwgraph-rs/blob/main/docs/packaging.md

## Microsoft Windows audio driver docs/samples

- https://learn.microsoft.com/en-us/windows-hardware/drivers/audio/acx-audio-class-extensions-overview
- https://learn.microsoft.com/en-us/windows-hardware/drivers/audio/acx-reference
- https://github.com/microsoft/Windows-driver-samples/tree/main/audio/Acx
- https://github.com/microsoft/Windows-driver-samples/tree/main/audio/sysvad
- https://github.com/microsoft/Windows-driver-samples/tree/main/audio/simpleaudiosample

## Rust Windows drivers

- https://github.com/microsoft/windows-drivers-rs
- https://github.com/microsoft/Windows-rust-driver-samples

## Process loopback

- https://github.com/microsoft/Windows-classic-samples/tree/main/Samples/ApplicationLoopback
- https://learn.microsoft.com/en-us/windows/win32/api/audioclientactivationparams/ne-audioclientactivationparams-process_loopback_mode
- https://learn.microsoft.com/en-us/windows/win32/api/audioclientactivationparams/ns-audioclientactivationparams-audioclient_process_loopback_params

## Driver signing / testing

- https://learn.microsoft.com/en-us/windows-hardware/drivers/install/kernel-mode-code-signing-policy--windows-vista-and-later-
- https://learn.microsoft.com/en-us/windows-hardware/drivers/dashboard/code-signing-reqs
- https://learn.microsoft.com/en-us/windows-hardware/drivers/dashboard/driver-signing-offerings
- https://learn.microsoft.com/en-us/windows-hardware/test/hlk/
- https://learn.microsoft.com/en-us/windows-hardware/drivers/driversecurity/driver-security-checklist

---

# 39. Recommended Final Decision

Use this architecture:

```text
Rust ACX virtual audio driver
        +
existing Rust user-mode router
        +
safe process-loopback capture
        +
optional isolated per-app AudioPolicyConfig backend
```

Do not replace the current Windows backend.

The shortest path to major parity is:

```text
Process loopback
    -> single-app relay + RMS

Rust virtual audio driver
    -> virtual app sink + Relay Microphone

Both together
    -> per-app effects + qpwgraph-owned app routes

Undocumented policy API last
    -> one-click automatic application routing
```

This keeps the difficult and risky pieces isolated while letting every completed stage deliver a useful Windows feature.
