# QPWGraph-RS — Windows Full Feature Completion Plan for LLM Agents

> Repository: `nglmercer/qpwgraph-rs`
>
> Target: complete the remaining Windows feature gaps while preserving the current working architecture, keeping the portable app usable without the optional driver, and making the optional Windows audio driver production-ready.

---

## 0. Mission

Implement the remaining Windows features:

1. **Automatic per-application output switching**
2. **Production Microsoft-signed Windows audio driver pipeline**
3. **Driver Verifier / HLK / Secure Boot release validation**
4. **100% project-authored Rust Windows audio driver**
5. Finish the remaining Windows lifecycle/client validation gaps
6. Update stale documentation so it accurately reflects the implementation

The implementation must preserve all already-working Windows functionality.

---

## 0.1 Current verification snapshot (2026-09-12)

The repository-level Windows work is ahead of the original bootstrap wording:

- the driver source tree contains Rust runtime modules (`acx.rs`, `driver.rs`,
  `ffi.rs`, and `transport.rs`) plus the WDK-facing header wrapper; no
  project-authored `.c`, `.cc`, or `.cpp` runtime source remains;
- the WDK/ACX toolchain audit, ACX-enabled release build, package metadata,
  package staging, and Rust transport/EOS tests pass on the available PC;
- process-loopback recovery and application-relay restart/session probes pass
  without a virtual driver;
- private AudioPolicyConfig activation succeeds on the current Windows 10
  build, but the first live `GetPersistedDefaultAudioEndpoint` call returns
  `E_INVALIDARG`; the backend therefore demotes automatic routing to
  `ManualOnly` and the automatic-switching acceptance rows remain open;
- live candidate-driver acceptance is still open: the installed development
  devnode is currently rejected by Windows as `CM_PROB_UNSIGNED_DRIVER`, and
  Verifier, lifecycle, HLK, Secure Boot, Microsoft signing, and ordinary-client
  acceptance evidence remain external release gates.

These results are source/build and host-mode evidence only. They do not mark
the live driver rows below complete.

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
[ ] driver loads
[ ] device starts
[ ] device stops
[ ] device remove works
[ ] no C runtime callback remains for these operations
```

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
[ ] four circuits enumerate
[ ] semantic roles are preserved
[ ] duplicate roles fail closed
[ ] wrong flow/role combinations fail closed
```

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
[ ] render endpoints open shared mode
[ ] capture endpoints open shared mode
[ ] unsupported formats fail safely
[ ] jack metadata still appears
```

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
[ ] open
[ ] start
[ ] pause
[ ] resume
[ ] stop
[ ] reset
[ ] reopen
[ ] no leaked stream context
```

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

### Phase R9 — Remove C implementation

After all live tests pass:

```text
[x] confirm no project-authored C/C++ runtime source remains
[x] confirm the project-authored C runtime build path is absent
[x] ensure `cargo check --features acx`
[x] ensure package build
[ ] ensure test-signed live install
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
[ ] unsupported build -> ManualOnly
[ ] unknown IID -> ManualOnly
[ ] ABI mismatch -> ManualOnly
[ ] stale PID -> reject
[ ] display-name-only selector -> reject
[ ] duplicate live selector match -> reject
[ ] user manual override prevents unsafe restore
```

Live:

```text
[ ] unpackaged Win32 app auto-moves
[ ] packaged MSIX app auto-moves
[ ] app route confirms isolation
[ ] app effects activate only after isolation
[ ] app restart re-applies route
[ ] qpwgraph restart reconciles safely
[ ] disabling rule restores previous endpoint
[ ] user manual override is preserved
[ ] unsupported Windows build falls back to manual mode
```

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
[ ] device disable/enable
[ ] AudioSrv restart
[ ] qpwgraph crash during active stream
[ ] render client crash
[ ] capture client crash
[ ] reboot
[ ] repeated install/uninstall
[ ] repeated upgrade
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

# 11. Feature I — Remaining client matrix

## 11.1 Relay Microphone

Already validated with generic WASAPI, OBS, and browser clients.

Still add:

```text
[ ] Discord
```

Optional additional:

```text
[ ] Teams
[ ] Zoom
[ ] DAW
```

These are acceptance tests, not new architecture.

## 11.2 Application relay

Already validated with helper/browser paths.

Add:

```text
[ ] Chrome
[ ] VLC
```

Verify:

```text
local playback unchanged
only target process relayed
restart resolves stable app selector
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

## PR 18 — Remaining live client matrix

- Discord
- Chrome
- VLC
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
[ ] four ACX endpoints implemented in Rust
[ ] two independent Rust PCM cables
[ ] correct stream timing
[ ] EOS correct
[ ] power transitions correct
[ ] no cross-talk
```

## Automatic app switching

```text
[ ] private ABI isolated behind one module
[ ] default disabled
[ ] supported-build detection
[ ] automatic app -> Virtual Output
[ ] actual isolation confirmation
[ ] safe restore
[ ] user manual override preserved
[ ] manual fallback remains
```

## Driver release

```text
[ ] Driver Verifier clean
[ ] lifecycle stress clean
[ ] sleep/resume clean
[ ] disable/enable clean
[ ] reboot clean
[ ] relevant HLK tests pass
[ ] Secure Boot validation pass
[ ] Microsoft production signature obtained
[ ] returned package signature verified
[ ] full installer built
```

## Client acceptance

```text
[x] generic WASAPI Relay Microphone
[x] OBS Relay Microphone
[x] browser Relay Microphone
[ ] Discord Relay Microphone
[ ] Chrome app relay
[ ] VLC app relay
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
Discord / OBS / browser / DAW
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
