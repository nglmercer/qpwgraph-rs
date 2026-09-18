# QPWGraph Windows PipeWire-like Audio Routing Implementation

Repository:

```text
https://github.com/nglmercer/qpwgraph-rs
```

## Objective

Implement a complete Windows audio-routing architecture for `qpwgraph-rs` that provides a PipeWire-like user experience as far as Windows permits.

The primary target use case is:

```text
Music application ─┐
                   ├──> qpwgraph mixer/effects ──> Virtual Microphone ──> Discord
Physical microphone┘
```

The implementation must also support:

```text
Application audio
    │
    ├──> normal physical playback
    │
    └──> qpwgraph capture/mix
```

when Windows process-loopback capture is available, and:

```text
Application
    │
    ▼
QPWGraph Virtual Output
    │
    ▼
QPWGraph Virtual Monitor
    │
    ▼
qpwgraph RouterCore
```

as the isolation/fallback path.

The final architecture should behave conceptually like:

```text
                       QPWGraph Windows Audio Graph

 ┌─────────────────── SOURCES ─────────────────────┐
 │                                                 │
 │ Physical microphone ─────── WASAPI capture ──┐  │
 │                                              │  │
 │ Spotify ─── process loopback ────────────────┤  │
 │                                              │  │
 │ Firefox ─── process loopback ────────────────┤  │
 │                                              │  │
 │ Virtual Monitor ─────────────────────────────┤  │
 │                                              │  │
 └──────────────────────────────────────────────┼──┘
                                                │
                                                ▼
                                      ┌──────────────────┐
                                      │    RouterCore    │
                                      │                  │
                                      │ mix              │
                                      │ gain             │
                                      │ resampling       │
                                      │ channel mapping  │
                                      │ effects          │
                                      │ metering         │
                                      │ limiter          │
                                      └────────┬─────────┘
                                               │
                  ┌────────────────────────────┼───────────────────────┐
                  │                            │                       │
                  ▼                            ▼                       ▼
             Headphones                Relay Sink             other endpoints
                  │                            │
                  │                      virtual cable
                  │                            │
                  │                            ▼
                  │                  Relay Microphone
                  │                            │
                  │                            ▼
                  │                    Discord / OBS /
                  │                    Zoom / Teams
                  │
                  └────────────────────────────────────
```

---

# 1. Important architectural rule

Do **not** attempt to reproduce PipeWire inside the Windows kernel.

Windows Core Audio is not a mutable arbitrary patchbay equivalent to PipeWire.

The architecture must instead be:

```text
Windows APIs / virtual endpoints
              │
              ▼
        qpwgraph owns PCM
              │
              ▼
          RouterCore
```

All meaningful DSP and routing logic must remain in user mode.

## Kernel driver responsibilities

The optional virtual audio driver should only provide standard Windows audio endpoints and bounded PCM transport.

The driver may be responsible for:

* publishing audio endpoint devices;
* render/capture endpoint lifecycle;
* transporting PCM between paired virtual endpoints;
* timing/packet handling required by ACX/WaveRT;
* bounded buffers;
* device lifecycle;
* exposing endpoint identity and role metadata.

The driver must **not** implement:

* graph routing policy;
* application selection;
* arbitrary mixing;
* effect chains;
* user-configurable gain;
* EQ;
* noise reduction;
* limiter;
* application policy;
* graph persistence;
* routing decisions;
* resampling unless fundamentally required at the device boundary.

Those belong in user mode.

---

# 2. Preserve the existing architecture

Before modifying anything, inspect the existing implementation.

Relevant areas currently include:

```text
crates/pw-graph-backend/src/router/
crates/pw-graph-backend/src/windows/
crates/pw-graph-backend/src/windows_relay.rs

drivers/windows-audio/
drivers/windows-audio/core/
drivers/windows-audio/driver/
drivers/windows-audio/package/
drivers/windows-audio/tests/
```

Existing Windows files include concepts such as:

```text
windows/app_route_policy.rs
windows/app_route_reconciler.rs
windows/audio_policy_config.rs
windows/driver.rs
windows/driver_application_routes.rs
windows/driver_relay.rs
windows/effects.rs
windows/identity.rs
windows/process_capture.rs
windows/process_loopback.rs
windows/routing.rs
windows/virtual_device.rs
windows/worker.rs
```

The router already includes:

```text
router/buffer.rs
router/diagnostics.rs
router/endpoints.rs
router/engine.rs
router/format.rs
router/meter.rs
router/resample.rs
router/thread.rs
router/wasapi.rs
```

The virtual audio driver already includes:

```text
driver/src/acx.rs
driver/src/driver.rs
driver/src/ffi.rs
driver/src/transport.rs
```

Do not duplicate an existing subsystem under a new name.

Extend/refactor existing components where appropriate.

---

# 3. Do not regress Linux

Linux/PipeWire behavior is the reference for graph semantics.

Changes to shared APIs must preserve:

```text
PipeWire routing
PipeWire metering
effects
patchbay persistence
MIDI
relay
graph UI
node capabilities
```

Windows-specific behavior must remain behind appropriate platform boundaries.

Shared abstractions should only be introduced where there is genuinely shared behavior.

Do not force Windows-specific concepts into the PipeWire backend.

---

# 4. Desired virtual endpoint topology

Retain the four-endpoint design.

## Application isolation cable

```text
QPWGraph Virtual Output
        │
        │ virtual PCM cable A
        ▼
QPWGraph Virtual Monitor
```

Roles:

```text
Virtual Output
    Windows type: render endpoint
    purpose: application sends audio into qpwgraph

Virtual Monitor
    Windows type: capture endpoint
    purpose: qpwgraph reads audio written to Virtual Output
```

## Relay/output cable

```text
QPWGraph Relay Sink
        │
        │ virtual PCM cable B
        ▼
QPWGraph Relay Microphone
```

Roles:

```text
Relay Sink
    Windows type: render endpoint
    purpose: qpwgraph writes final mixed PCM

Relay Microphone
    Windows type: capture endpoint
    purpose: Discord/OBS/etc. see qpwgraph mix as microphone
```

The two cables must be completely independent.

Conceptually:

```text
Cable A:

APP
 │
 ▼
Virtual Output
 │
 ▼
Virtual Monitor
 │
 ▼
QPWGraph


Cable B:

QPWGraph
 │
 ▼
Relay Sink
 │
 ▼
Relay Microphone
 │
 ▼
Discord
```

---

# 5. Primary routing use case

Implement this exact scenario end-to-end:

```text
Spotify audio
        │
        ▼
ProcessLoopbackSource
        │
        │
        ├──────────────────────────────┐
        │                              │
        ▼                              │
    RouterCore                         │
        ▲                              │
        │                              │
Physical Microphone                    │
via WASAPI capture                     │
                                       │
RouterCore mix                         │
        │                              │
        ▼                              │
QPWGraph Relay Sink                    │
        │                              │
        ▼                              │
QPWGraph Relay Microphone              │
        │                              │
        ▼                              │
Discord                                │
                                       │
Spotify must still be independently ───┘
audible through its normal output
when using process loopback.
```

The user should eventually be able to construct this visually through the graph.

---

# 6. Process-loopback capture

Use the supported Windows process-loopback mechanism when available.

The existing Windows implementation already contains:

```text
process_capture.rs
process_loopback.rs
```

Audit those implementations before writing new code.

The logical abstraction should look approximately like:

```rust
struct ProcessLoopbackSource {
    process_id: u32,
    include_process_tree: bool,
    // COM/audio state
    // bounded PCM producer
    // diagnostics
}
```

The implementation should activate an audio client using process-loopback activation and capture PCM belonging to a target process.

Prefer:

```text
PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE
```

for normal application capture.

Do not capture unrelated system audio.

## Requirements

Process capture must:

* identify the correct target PID;
* optionally include the child process tree;
* survive normal buffer starvation;
* produce silence instead of replaying stale buffers;
* use bounded buffering;
* never block the router thread;
* detect target-process exit;
* report unsupported activation cleanly;
* report capture failures cleanly;
* expose useful diagnostics;
* cleanly stop and release COM objects;
* avoid leaking activation callbacks;
* handle device/audio-service changes.

## Threading

All COM interfaces must respect COM apartment/thread ownership.

A worker that creates a COM audio interface should normally own/use/release it from the appropriate worker thread.

Do not casually move COM interfaces across unrelated threads.

---

# 7. Runtime capability detection

Do not make application behavior depend only on hardcoded Windows-version strings.

Implement capability detection.

Conceptually:

```rust
enum ProcessCaptureCapability {
    Available,
    UnsupportedOs,
    ActivationFailed(HResult),
    TargetUnavailable,
}
```

The application should be able to distinguish:

```text
API not supported
target application disappeared
temporary WASAPI failure
invalid PID
audio service unavailable
capture active
```

Capability probes may be cached, but caches must be invalidatable following significant audio-service/device changes.

---

# 8. Application identity

Do not use PID as the persistent application identity.

PIDs are ephemeral.

Use the existing Windows identity infrastructure.

A stable application selector should prefer, when available:

```text
package identity
application/user model identity
executable path
process image identity
session metadata
```

PID should only identify the currently running process instance.

Conceptually:

```rust
struct ApplicationSelector {
    stable_identity: ...,
}

struct LiveApplication {
    selector: ApplicationSelector,
    pid: u32,
}
```

If a stable selector can no longer resolve to a live process:

```text
DO NOT silently attach to another unrelated process.
```

Fail closed.

---

# 9. Physical microphone capture

Physical capture endpoints should use the existing WASAPI abstraction.

Do not implement a second capture engine specifically for microphone mixing.

Reuse or extend:

```text
router/wasapi.rs
router/endpoints.rs
windows/routing.rs
```

A microphone route must become an ordinary router source.

Example:

```text
Physical Microphone
        │
        ▼
WasapiCaptureSource
        │
        ▼
bounded ring
        │
        ▼
RouterCore
```

---

# 10. RouterCore remains the audio engine

All sources should converge into the existing router.

Conceptual source types:

```text
PhysicalCaptureSource
RenderLoopbackSource
ProcessLoopbackSource
VirtualMonitorSource
RelaySource
```

Conceptual sinks:

```text
PhysicalRenderSink
VirtualRenderSink
RelaySink
```

Do not create a special mixer solely for Discord.

Discord should simply consume an ordinary qpwgraph route through the virtual microphone.

---

# 11. Internal PCM format

The router may preserve its existing internal format strategy.

If normalization is useful, prefer a predictable float representation such as:

```text
PCM: f32
typical graph rate: 48 kHz
channel representation: explicit
```

Do not assume all endpoints use:

```text
48 kHz
stereo
float
```

Inputs may be:

```text
44.1 kHz stereo
48 kHz mono
48 kHz 5.1
integer PCM
float PCM
different channel masks
```

Use the existing router conversion path.

Do not add format conversions inside arbitrary feature code.

---

# 12. Mixing

Support many-to-one routing.

Example:

```text
Spotify ────────┐
                │
Firefox ────────┼──> QPWGraph Relay Microphone
                │
Microphone ─────┘
```

Each source needs independent:

```text
gain
mute
metering
effects where applicable
```

Summation must not cause undefined overflow/NaN behavior.

Sanitize non-finite samples.

A final optional limiter/soft protection stage may be added if it integrates cleanly with the existing effect architecture.

Do not silently normalize every source.

User-selected gain must remain deterministic.

---

# 13. Fan-out

One source must be usable by multiple destinations.

Example:

```text
Spotify capture
     │
     ├──> headphones
     │
     ├──> Discord mix
     │
     └──> recorder
```

Do not capture the same process independently for every destination if a single source can feed multiple router branches.

Pull a logical source once per processing cycle and fan out inside the router.

---

# 14. Effects

Effects belong in the user-mode route graph.

Example:

```text
Microphone
    │
    ▼
Noise reduction
    │
    ▼
EQ
    │
    ├──> headphones monitor
    │
    └──> Discord microphone
```

Effects must never move into the kernel driver.

Preserve existing per-branch effect semantics.

An effect inserted in one branch must not unintentionally modify sibling routes.

---

# 15. Metering

Use actual PCM whenever qpwgraph owns the samples.

For owned PCM calculate at least:

```text
peak
RMS
```

Do not pretend Windows Core Audio peak meters provide true RMS.

Metering must correctly identify whether the value came from:

```text
native endpoint/session peak meter

or

actual router PCM
```

When the router owns PCM, router measurements should be authoritative for that route.

---

# 16. Virtual Output isolation path

Implement/support the following flow:

```text
Spotify
    │
    ▼
QPWGraph Virtual Output
    │
    ▼
virtual cable A
    │
    ▼
QPWGraph Virtual Monitor
    │
    ▼
RouterCore
```

Then allow:

```text
Virtual Monitor
      │
      ├──> physical headphones
      │
      └──> Relay mix
```

This provides true isolation because qpwgraph owns the stream after the application renders into the virtual endpoint.

This path is required for scenarios where:

* process-loopback capture is unavailable;
* the user wants complete rerender ownership;
* local effects must replace the dry application path;
* automatic/manual per-application endpoint assignment is used.

---

# 17. Avoid dry + processed duplication

For application effects such as:

```text
Spotify -> EQ -> headphones
```

do not create:

```text
original Spotify -> headphones

PLUS

captured Spotify -> EQ -> headphones
```

which would produce doubled/echoing audio.

Application rerender/effects must only be activated once isolation has been established.

Preferred isolation:

```text
Spotify
    │
    ▼
Virtual Output
    │
    ▼
Virtual Monitor
    │
    ▼
EQ
    │
    ▼
headphones
```

Process loopback alone is suitable for:

```text
read-only capture
recording
metering
relay mixing
Discord mix
```

but not necessarily replacing the application's original playback path.

---

# 18. Application routing policy

The repository already contains:

```text
app_route_policy.rs
app_route_reconciler.rs
audio_policy_config.rs
driver_application_routes.rs
```

Audit this code before changing it.

Automatic reassignment of an application's Windows output device must remain optional and fail closed.

Do not make undocumented/private Windows audio-policy APIs a requirement for normal operation.

The hierarchy should be:

```text
1. Process loopback when read-only application capture is enough.

2. User manually selects:
   QPWGraph Virtual Output
   in Windows application audio settings.

3. Optional automatic application reassignment where the existing
   policy backend is explicitly verified and enabled.
```

The user must always have a supported/manual fallback.

---

# 19. Automatic application isolation transaction

If automatic endpoint assignment is enabled, treat it as a transaction.

Pseudo-flow:

```text
resolve stable application identity
        │
        ▼
find live process/session
        │
        ▼
confirm QPWGraph Virtual Output exists
        │
        ▼
request app endpoint change
        │
        ▼
refresh Core Audio sessions
        │
        ▼
verify application is actually isolated
        │
        ├── no ──> rollback / ManualOnly
        │
        ▼
start Virtual Monitor route
        │
        ▼
enable effects/rerender
```

Never report the route as active before verifying isolation.

On failure:

```text
preserve original playback
do not create duplicate processed audio
show useful diagnostic
fall back to manual routing
```

---

# 20. Route leases and restoration

If qpwgraph automatically changes application output assignment, it must know which changes it owns.

Implement/retain route ownership/lease semantics.

Example:

```text
original:
Spotify -> Speakers

qpwgraph:
Spotify -> Virtual Output

on cleanup:
restore Spotify -> Speakers
```

But only restore settings that qpwgraph itself changed.

If the user manually changes the application output while qpwgraph is running, do not blindly overwrite the user's choice during shutdown.

---

# 21. Relay Sink integration

The user-mode router must be able to render into:

```text
QPWGraph Relay Sink
```

through normal WASAPI rendering.

Conceptually:

```text
RouterCore
    │
    ▼
WasapiRenderSink
    │
    ▼
QPWGraph Relay Sink
```

The application should discover the endpoint by stable provider identity/role rather than fragile display-name matching wherever possible.

Display names may be used as diagnostics/UI labels, not as the sole identity.

---

# 22. Relay Microphone

The driver must expose:

```text
QPWGraph Relay Microphone
```

as a standard Windows capture endpoint.

Third-party applications must be able to use it without special SDKs.

Examples:

```text
Discord
OBS
Zoom
Teams
DAWs
browsers
games
```

From their perspective it is an ordinary microphone.

They must not need to know that qpwgraph exists.

---

# 23. Driver transport

The driver currently contains bounded Rust PCM transport infrastructure.

Preserve the minimal two-cable concept.

Conceptually:

```text
Cable A:
render endpoint -> capture endpoint

Cable B:
render endpoint -> capture endpoint
```

Required properties:

```text
bounded memory
no unbounded allocation
predictable packet timing
clear underrun behavior
clear overrun behavior
silence when data is absent
no stale sample replay
independent cable state
safe lifecycle reset
```

The two cables must not share audio accidentally.

---

# 24. Driver endpoint roles

Preserve explicit provider-owned endpoint role metadata.

Expected logical roles:

```text
app-render
app-monitor
relay-render
relay-capture
```

The user-mode application should use these roles to locate the correct devices.

Do not rely exclusively on localized endpoint display names.

---

# 25. Driver failure isolation

A driver issue must not corrupt RouterCore state.

If a virtual endpoint disappears:

```text
route remains represented
route enters degraded state
diagnostic is recorded
worker stops safely
backend attempts reconciliation later
```

Do not silently delete user patchbay intent merely because a device temporarily disappeared.

---

# 26. Device invalidation

Handle:

```text
AUDCLNT_E_DEVICE_INVALIDATED
device removal
audio service restart
endpoint disable/enable
default device changes
sleep/resume
driver reinstall
virtual endpoint disappearance
```

Audio workers should notify the control plane.

Do not attempt complex graph reconstruction inside an audio callback.

Expected flow:

```text
audio worker detects invalidation
        │
        ▼
publish lightweight fault
        │
        ▼
control/backend refresh
        │
        ▼
tear down affected worker
        │
        ▼
re-resolve endpoint identity
        │
        ▼
recreate worker
        │
        ▼
reset buffers/effects across discontinuity
```

---

# 27. Real-time rules

The router audio thread must remain real-time friendly.

Inside audio processing:

Do not:

```text
allocate dynamically
block on mutexes
perform filesystem operations
perform COM enumeration
format strings
log synchronously
perform network operations
wait on UI
sleep
```

Use:

```text
bounded queues
preallocated buffers
atomics
lock-free/single-producer-single-consumer structures where appropriate
control messages between blocks
```

Keep existing real-time invariants.

---

# 28. Buffering

Every boundary must use bounded buffering.

Never solve underruns by allowing latency to grow forever.

Required behavior:

```text
source too slow:
    output silence for unavailable samples
    count underrun/starvation

consumer too slow:
    drop according to existing bounded-buffer policy
    count overrun/drop

never:
    infinitely grow memory
```

---

# 29. Clock drift

Physical devices do not share a perfect clock.

Example:

```text
mic: nominal 48000 Hz
headphones: nominal 48000 Hz
actual clocks differ slightly
```

Retain/use router drift compensation.

Do not assume equal nominal sample rates imply synchronous clocks.

Expose diagnostics such as:

```text
resampler ratio
buffer fill
clock drift ppm
underruns
overruns
```

---

# 30. UI/graph model

The Windows graph should distinguish:

```text
observed Core Audio relationship

versus

qpwgraph-owned mutable route
```

Observed Windows application session relationships must not falsely appear fully mutable.

Nodes should expose accurate per-node capabilities.

Examples:

```text
ordinary app session:
    process capture: maybe yes
    direct native reroute: no
    qpwgraph virtual isolation: maybe
    peak meter: maybe
    true RMS: only when PCM captured

physical microphone:
    source route: yes

physical speakers:
    destination route: yes
    loopback source: yes

Virtual Monitor:
    source: yes

Relay Sink:
    destination: yes

Relay Microphone:
    externally consumable capture endpoint
```

---

# 31. Application node routing UX

An application session should expose enough information for the graph to offer meaningful actions.

Possible conceptual ports:

```text
Spotify
 ├── observed playback -> Speakers
 └── capture output -> process loopback
```

If isolated:

```text
Spotify
    │
    ▼
Virtual Output
    │
    ▼
Virtual Monitor
```

Do not represent an unsupported drag operation as if Windows will natively rewire it.

---

# 32. Desired Discord workflow

The ideal user workflow should become:

```text
1. Start qpwgraph.

2. qpwgraph discovers:
   Spotify
   microphone
   headphones
   Relay Microphone

3. User connects:
   Spotify capture -> Relay Sink
   microphone -> Relay Sink

4. Optional:
   Spotify -> headphones
   microphone -> effects -> Relay Sink

5. In Discord:
   Input Device = QPWGraph Relay Microphone
```

No external mixer should be required once the qpwgraph virtual driver is installed.

---

# 33. Development fallback without custom driver

The application architecture must remain testable before the custom driver is production-ready.

Permit testing using an external virtual cable during development.

Conceptually:

```text
ProcessLoopbackSource
        │
        │
PhysicalMic
        │
        ▼
RouterCore
        │
        ▼
generic WASAPI render endpoint
        │
        ▼
external virtual cable
        │
        ▼
Discord
```

Do not make such third-party cable software a production dependency.

It is only useful for isolating user-mode bugs from driver bugs.

---

# 34. Routing abstraction

Prefer generic source/sink interfaces.

Example shape:

```rust
trait AudioSource {
    fn format(&self) -> AudioFormat;
    fn read(&mut self, dst: &mut [f32]) -> SourceRead;
}

trait AudioSink {
    fn format(&self) -> AudioFormat;
    fn write(&mut self, src: &[f32]) -> SinkWrite;
}
```

Do not necessarily introduce these exact traits if equivalent abstractions already exist.

Reuse the existing architecture.

The important requirement is that RouterCore should not care whether audio came from:

```text
physical mic
process loopback
render loopback
virtual monitor
network relay
```

---

# 35. Diagnostics

Every active route/source/sink should have useful diagnostics.

At minimum consider:

```text
frames processed
source underruns
source overruns
sink underruns
sink overruns
dropped frames
discontinuities
restarts
buffer depth
sample rate
channel count
resampler ratio
clock drift
last HRESULT / fault category
router processing time
effect processing time
process PID
stable app identity
endpoint ID
```

Do not pass formatted diagnostic strings through real-time paths.

Use enums/codes/atomics and format on the control/UI side.

---

# 36. Error model

Avoid generic string-only errors for audio lifecycle.

Prefer typed errors.

Example:

```rust
enum ProcessCaptureError {
    Unsupported,
    ActivationFailed(HRESULT),
    ProcessGone,
    AudioServiceUnavailable,
    ClientInitializeFailed(HRESULT),
    CaptureClientFailed(HRESULT),
}
```

And route faults such as:

```rust
enum RouteFault {
    None,
    SourceStarved,
    SourceInvalidated,
    SinkInvalidated,
    ProcessExited,
    Unsupported,
    FormatNegotiationFailed,
    DriverUnavailable,
}
```

Reuse existing error types where possible instead of duplicating them.

---

# 37. Do not silently fall back to system-loopback

If the user selects:

```text
Spotify
```

but process capture fails, do not silently capture:

```text
all system audio
```

That would violate routing intent.

Instead:

```text
show process capture unavailable

offer/use Virtual Output isolation

or require explicit user fallback selection
```

Fail closed.

---

# 38. Privacy semantics

Application capture must always correspond to an explicit graph/user selection.

Do not automatically capture all process audio simply because the API allows it.

Routing state must clearly identify what process/application is being captured.

---

# 39. Patchbay persistence

Persist logical intent using stable identities.

Do not persist transient values such as:

```text
PID
MMDevice numeric enumeration index
WinMM transient index
```

Persist:

```text
stable endpoint ID
provider role
stable app identity
effect instance identity
logical route
```

On restore:

```text
resolve currently available devices/apps
restore what can be restored
leave unavailable intent visible/degraded where appropriate
never attach to a different app merely because an identifier was reused
```

---

# 40. Startup behavior

Recommended initialization sequence:

```text
initialize backend

enumerate physical endpoints

enumerate virtual endpoint roles

enumerate application sessions

resolve stable identities

probe process-loopback capability

initialize router thread

restore owned routes

restore patchbay intent

start required source/sink workers

publish graph snapshot
```

Driver absence must not prevent the rest of qpwgraph from starting.

Without the driver, features requiring third-party-visible virtual endpoints should simply be unavailable.

---

# 41. Shutdown behavior

Shutdown order must be deterministic.

Preferred conceptual order:

```text
stop accepting new graph mutations

remove router routes

stop process capture workers

stop WASAPI workers

stop virtual endpoint workers

restore application route leases owned by qpwgraph

drop router resources

unregister callbacks

release COM resources

exit
```

Do not allow worker threads to outlive state they reference.

---

# 42. Testing strategy

Implement tests in layers.

## Router tests

No Windows audio hardware required.

Test:

```text
mixing two sources
fan-out
gain
mute
channel mapping
resampling
effects
branch effects
metering
RMS
peak
source starvation
sink backpressure
NaN/Inf sanitization
transactional route updates
device loss simulation
clock drift behavior
```

---

# 43. Process-capture tests

Where possible, separate:

```text
activation parameter construction
lifecycle state machine
stable selector resolution
buffer handoff
worker shutdown
fault propagation
```

from actual live Windows audio activation.

These parts should be unit-testable.

Live tests must be opt-in where needed.

---

# 44. Virtual driver tests

Preserve and extend existing smoke tests.

Test:

```text
all four endpoint roles exist

app-render -> app-monitor round trip

relay-render -> relay-capture round trip

the two cables are isolated from each other

silence behavior

non-silent PCM integrity

format setup

client disconnect

client reconnect

audio service restart

endpoint disable/enable

device uninstall

device reinstall

sleep/resume

client crash

driver stress
```

---

# 45. End-to-end acceptance test: Discord-style mix

Create a reproducible test scenario equivalent to:

```text
source A:
    generated stereo sine or test process audio

source B:
    generated microphone-like mono signal

route:
    A + B -> Relay Sink

capture:
    Relay Microphone

verify:
    both signals exist in captured stream
    expected gain relationship
    no unrelated system audio
    no stale frames
```

This test should not require Discord itself.

Discord is only a real-world compatibility client.

---

# 46. End-to-end application capture acceptance test

Create an application/process that renders a known test signal.

Then verify:

```text
target process
    │
    ▼
ProcessLoopbackSource
    │
    ▼
RouterCore
    │
    ▼
test sink
```

Requirements:

```text
known tone is captured

unrelated process tone is not captured

process exit is detected

restart can resolve the application again through stable identity
```

---

# 47. Isolation acceptance test

Verify:

```text
test application
    │
    ▼
QPWGraph Virtual Output
    │
    ▼
QPWGraph Virtual Monitor
    │
    ▼
RouterCore
    │
    ▼
physical/test sink
```

Confirm there is no duplicate dry path produced by qpwgraph.

---

# 48. No-driver mode

Explicitly test qpwgraph without the custom virtual driver installed.

Expected:

```text
physical endpoint routing works

process-loopback capture works when supported

relay/network features that do not require virtual microphone remain usable

virtual endpoint features show unavailable state

application does not crash

graph does not expose fake endpoints
```

---

# 49. CI requirements

Do not require:

```text
physical audio hardware
installed virtual driver
interactive desktop
Discord
```

for ordinary CI.

Separate:

```text
unit tests
compile tests
live Windows audio smoke tests
driver validation
HLK/Verifier/release gates
```

Live hardware/driver tests should remain explicit opt-in acceptance tests.

---

# 50. Driver release gates

Do not treat:

```text
cargo build succeeded
```

as proof that the driver is production-ready.

Preserve/enforce appropriate driver validation including the existing workflow around:

```text
test signing
install/uninstall
smoke tests
Driver Verifier
lifecycle validation
HLK
Microsoft signing
Secure Boot
upgrade behavior
client crash behavior
audio service restart
```

Development packages must fail closed when required validation state is absent.

---

# 51. Implementation phases

Implement incrementally.

## Phase 0 — baseline

Before changing code:

```bash
cargo fmt --all -- --check
cargo test --workspace --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

Record existing failures separately.

Do not blame pre-existing failures on new work.

---

## Phase 1 — audit existing Windows audio code

Inspect:

```text
process_loopback.rs
process_capture.rs
routing.rs
router/wasapi.rs
virtual_device.rs
driver_relay.rs
driver_application_routes.rs
app_route_policy.rs
app_route_reconciler.rs
```

Document internally what already works before replacing anything.

Delete no working code merely because a new abstraction looks cleaner.

---

## Phase 2 — complete ProcessLoopbackSource

Ensure single-application PCM capture works end-to-end.

Definition of done:

```text
live PID can be captured
process tree mode works
bounded PCM reaches RouterCore
process exit is detected
clean cancellation works
unsupported systems fail cleanly
diagnostics work
tests exist
```

---

## Phase 3 — physical mic + process audio mixing

Implement:

```text
Physical mic ─┐
              ├──> RouterCore
Process app ──┘
```

Definition of done:

```text
both streams audible in test sink
independent gain
independent mute
metering works
sample-rate mismatch works
mono/stereo mapping works
```

---

## Phase 4 — Relay Sink output

Route RouterCore into:

```text
QPWGraph Relay Sink
```

Definition of done:

```text
WASAPI can open relay-render
router continuously writes PCM
device loss propagates correctly
```

If custom driver is not available on the development machine, first validate this layer against a generic WASAPI render endpoint.

---

## Phase 5 — Relay Microphone cable

Finish/validate:

```text
Relay Sink
    │
    ▼
Relay Microphone
```

Definition of done:

```text
standard Windows capture clients can open Relay Microphone
PCM written to Relay Sink is captured correctly
no unbounded latency
two independent client lifecycles work
```

---

## Phase 6 — Virtual Output isolation cable

Finish/validate:

```text
Virtual Output
    │
    ▼
Virtual Monitor
```

Definition of done:

```text
standard Windows render clients can select Virtual Output
qpwgraph can capture corresponding PCM from Virtual Monitor
app and relay cables remain independent
```

---

## Phase 7 — graph integration

Expose appropriate Windows graph nodes and ports.

Definition of done:

```text
process source appears
virtual monitor appears
relay sink appears
physical mic appears
physical outputs appear

valid links are draggable
invalid Windows-native rewires are not falsely advertised
```

---

## Phase 8 — patchbay persistence

Persist and restore Windows-owned routes using stable identities.

Definition of done:

```text
restart qpwgraph
owned routes restore
missing endpoints do not bind incorrectly
missing apps remain unresolved instead of attaching to another process
```

---

## Phase 9 — optional automatic application routing

Only after the manual isolation path works.

Definition of done:

```text
explicit opt-in
supported configuration detected
assignment verified after request
failure becomes ManualOnly
owned changes restored safely
never required for core routing
```

---

## Phase 10 — resilience

Validate:

```text
device removal
audio service restart
app exit
app restart
default-device switch
virtual driver restart
sleep/resume
```

No deadlocks.

No stale audio loops.

No orphan worker threads.

---

# 52. Expected final functional scenarios

All of the following should work.

## Scenario A

```text
Microphone -> Discord
```

through:

```text
Mic -> RouterCore -> Relay Sink -> Relay Microphone -> Discord
```

---

## Scenario B

```text
Spotify + Microphone -> Discord
```

through:

```text
Spotify ProcessLoopback ─┐
                         ├──> RouterCore
Physical Mic ────────────┘
                              │
                              ▼
                         Relay Sink
                              │
                              ▼
                       Relay Microphone
                              │
                              ▼
                           Discord
```

---

## Scenario C

Spotify remains audible locally while being sent to Discord:

```text
Spotify normal playback ─────────────> headphones
      │
      └── process loopback copy
                    │
                    ▼
               RouterCore
                    │
                    ▼
                 Discord
```

---

## Scenario D

Application effects replacing original output:

```text
Spotify
    │
    ▼
Virtual Output
    │
    ▼
Virtual Monitor
    │
    ▼
EQ / effects
    │
    ▼
headphones
```

---

## Scenario E

Application effects plus Discord:

```text
Spotify
    │
    ▼
Virtual Output
    │
    ▼
Virtual Monitor
    │
    ├──> EQ -> headphones
    │
    └──> gain -> Relay mix -> Discord
```

---

## Scenario F

Multiple applications:

```text
Spotify ─┐
Firefox ─┼──> Discord
Game ────┤
Mic ─────┘
```

Each with independent routing/gain.

---

# 53. Code-quality requirements

Do not produce placeholder implementations such as:

```rust
todo!()
unimplemented!()
panic!("not implemented")
```

for runtime code that is part of a completed phase.

Do not hide errors using:

```rust
let _ = dangerous_operation();
```

unless intentionally ignoring an error is justified and documented.

Avoid unnecessary `unsafe`.

Every `unsafe` block should have a clear invariant.

For COM/Win32 FFI:

```text
validate pointer lifetime
validate structure size
validate buffer ownership
validate async callback ownership
validate thread/apartment ownership
```

---

# 54. Do not rewrite the whole backend

Prefer focused changes.

Do not replace the current router.

Do not replace the graph model.

Do not replace the effects system.

Do not replace working Windows Core Audio enumeration.

Do not replace the driver workspace unless a concrete defect requires it.

This task is primarily about completing/integrating the architecture already present.

---

# 55. Documentation changes

Update relevant documentation after implementation.

At minimum review:

```text
docs/platform-parity.md
docs/audio-router.md
docs/features.md
docs/building.md
docs/configuration.md
drivers/windows-audio/package/README.md
```

Documentation must distinguish:

```text
works without driver
works with driver
process-loopback capability
manual application isolation
automatic experimental isolation
driver release validation state
```

Do not claim a feature has been live validated unless corresponding evidence exists.

---

# 56. Required final agent report

When implementation is complete, report:

```text
1. Architecture implemented

2. Files changed

3. Existing code reused

4. New abstractions introduced

5. Process-loopback status

6. Virtual Output/Monitor status

7. Relay Sink/Microphone status

8. App routing-policy status

9. Windows versions/capabilities tested

10. Tests added

11. Tests executed

12. Driver validation executed

13. Remaining live-validation requirements

14. Known limitations
```

Do not merely say:

```text
"implemented successfully"
```

Provide evidence.

---

# 57. Validation commands

At repository root run:

```bash
cargo fmt --all -- --check
cargo test --workspace --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

Also run relevant Windows-specific tests.

For the nested driver workspace, use the repository's documented driver commands and validation workflow.

Do not claim kernel-driver validation from ordinary Cargo tests alone.

---

# 58. Critical invariants

These are mandatory.

```text
1. RouterCore owns application-level routing.

2. DSP stays in user mode.

3. Kernel driver remains minimal.

4. All PCM queues are bounded.

5. Audio threads do not block.

6. No stale sample replay after starvation.

7. Application identity is not persisted by PID.

8. Process-capture failure never silently becomes whole-system capture.

9. Effects that replace app playback require isolation.

10. Automatic app reassignment is optional.

11. Manual routing remains a valid fallback.

12. Driver absence does not prevent qpwgraph startup.

13. Linux behavior is not regressed.

14. Observed Windows session relationships are not falsely represented
    as arbitrary mutable PipeWire links.

15. User-owned Windows settings are not blindly overwritten on cleanup.

16. Device loss is recoverable.

17. Virtual app and relay cables remain independent.

18. Third-party clients see Relay Microphone as an ordinary Windows mic.

19. Real-time processing does not allocate or perform blocking control work.

20. Tests prove behavior rather than merely proving compilation.
```

---

# 59. Preferred final architecture

The implementation should converge on this:

```text
                            WINDOWS APPLICATIONS

      Spotify               Firefox               Game
         │                     │                    │
         ├──── Process Loopback┼────────────────────┤
         │                     │                    │
         │                     │                    │
         │ optional isolation  │                    │
         ▼                     ▼                    ▼
   Virtual Output         Virtual Output       Virtual Output
         │
         ▼
   Virtual Monitor
         │
         │
         ├────────────────────────────┐
         │                            │
         ▼                            │
 ┌──────────────────────────────────────────────┐
 │                   RouterCore                 │
 │                                              │
 │ sources                                      │
 │   process loopback                           │
 │   physical capture                          │
 │   render loopback                            │
 │   virtual monitor                            │
 │                                              │
 │ processing                                   │
 │   gain                                       │
 │   mute                                       │
 │   channel map                                │
 │   resampling                                 │
 │   effects                                    │
 │   peak/RMS                                   │
 │   mix                                        │
 │                                              │
 │ routing                                      │
 │   fan-in                                     │
 │   fan-out                                    │
 │   branches                                   │
 └──────┬──────────────────────┬────────────────┘
        │                      │
        ▼                      ▼
  Physical output        Relay Sink
                               │
                               │ virtual cable
                               ▼
                         Relay Microphone
                               │
             ┌─────────────────┼────────────────┐
             ▼                 ▼                ▼
          Discord             OBS             Zoom
```

---

# 60. Agent execution instruction

Do not stop after producing an implementation plan.

Inspect the repository and implement the changes.

Work incrementally and keep the repository compiling between logical phases where practical.

Before creating a new subsystem, search the repository for an existing equivalent.

When existing code partially implements a requirement:

```text
complete it
test it
integrate it
```

rather than duplicating it.

If live Windows driver validation cannot be performed in the current environment:

```text
implement everything that can be implemented statically/unit-tested,
run all available checks,
clearly mark only the live validation as unresolved,
and do not weaken fail-closed driver behavior just to make tests pass.
```

The desired outcome is not merely a Windows visualization of audio sessions.

The desired outcome is a real qpwgraph-owned Windows PCM routing system capable of:

```text
application audio
+
microphone
+
effects
+
mixing
+
fan-out
+
virtual microphone output
```

with a user experience as close as reasonably possible to PipeWire while respecting Windows audio architecture.
