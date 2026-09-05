# Windows feature parity plan

Status snapshot: 2026-09-04, commit `fd67694`.

This is the short execution plan for Windows parity. A checked item means the
repository contains the corresponding source and offline checks; it does not
claim that a live Windows audio machine, WDK VM, Driver Verifier, HLK, or
signing service has been run.

## Goal

Keep the existing Rust user-mode router as the PCM engine while making Windows
support the useful parts of the Linux/PipeWire experience:

- real endpoint-to-endpoint routing, effects, gain, and RMS;
- safe process-loopback capture and single-application relay;
- optional qpwgraph-owned virtual endpoints, including Relay Microphone;
- stable configuration, diagnostics, recovery, packaging, and release testing.

The kernel driver must transport ordinary audio streams only. Mixing, effects,
resampling, relay policy, persistence, and UI remain in user mode.

## Current status

| Area | Status | Evidence / limit |
| --- | --- | --- |
| Core Audio graph, sessions, controls, peak meters | Done | `crates/pw-graph-backend/src/windows/` |
| WinMM MIDI enumeration and mutable routing | Done | Existing Windows backend and parity tests |
| Physical WASAPI capture, loopback, render routing | Done | `router/wasapi.rs`, `windows/routing.rs` |
| Router effects, software gain, true RMS, route counters | Done | `router/`, Windows route integration |
| Process loopback | Partial | Lifetime-safe source exists; only sessions already on `QPWGraph Virtual Output` are eligible; live activation is unverified here |
| Single-application relay | Partial | Works through the same manual virtualized-session gate |
| Virtual audio driver | Scaffold only | Nested workspace, ring core, INF/manifest, installer guards; `EvtDeviceAdd` still returns `STATUS_NOT_SUPPORTED` |
| Virtual render/capture cable and Relay Microphone | Missing | Requires ACX/KMDF endpoint implementation |
| Application route persistence | Schema only | Stable selectors and TOML fields exist; refresh/startup restore is not wired |
| Automatic per-application output routing | Unsupported by default | Public Core Audio API is absent; private ABI is isolated behind `AppRoutePolicy` |
| Diagnostics | Partial | Bounded Windows report and route counters exist; dedicated UI panel/badges are not complete |
| Release validation | Partial | User-mode CI and pure-Rust driver tests pass; WDK/VM/Verifier/HLK/signing remain external gates |

## Rules that must not regress

1. Ordinary application session links remain observed and immutable unless
   Windows reports that the session is already isolated on qpwgraph's virtual
   output.
2. Never persist a PID as application identity. Use executable/package
   selectors and verify the live identity immediately before activation.
3. Process-loopback activation owns its boxed activation parameters, blob,
   `PROPVARIANT`, completion state, and async operation until completion.
4. A lost endpoint is reported to the control plane. Reopen workers outside the
   paced audio thread and preserve links/effects/gain across recovery.
5. The optional driver is fail-closed: no fake endpoint, no default-device
   takeover, no installer success before an ACX implementation is ready.
6. Never probe undocumented vtable slots. An experimental policy backend must
   use verified declarations, an explicit opt-in, runtime IID checks, and a
   manual Windows Volume Mixer fallback.
7. No realtime allocation, unbounded queue, formatting, or blocking mutex in a
   driver/audio callback.

## Implementation phases

### Phase 0 — Baseline and contract (complete)

- Keep Windows user-mode builds feature-matrix checked in CI.
- Keep platform-neutral graph, meter, gain, and route-ownership contracts in
  shared tests.
- Keep the driver workspace excluded from the portable application workspace.

### Phase 1 — Finish repository-only application routing

Files: `pw-graph-config`, `windows/app_route_policy.rs`,
`windows/worker.rs`, `windows/routing.rs`.

- Resolve a live session by stable selector, never by saved PID.
- Apply `WindowsApplicationRoute` after refresh/startup when the selector,
  endpoint, and virtualization proof are all present.
- Restore destination, effect chain, gain, and enabled state through the same
  owned-route path used for manual connections.
- Mark the rule inactive/degraded when the process, endpoint, or process-loopback
  capability is unavailable; retry when the application returns.
- Add tests for selector matching, PID reuse, duplicate-audio refusal, route
  restoration, and missing-device recovery.

Exit condition: a saved rule can safely reattach to the same executable after a
restart without ever routing an unrelated PID.

### Phase 2 — Finish diagnostics and capability UI

Files: `windows/driver.rs`, `pw-graph-app-core`, `pw-graph-slint`.

- Extend the bounded Windows report with process selector, PID/generation,
  process-loopback state, endpoint health, last HRESULT, and route fault data.
- Add a user-visible “Copy Windows audio report” action.
- Show capability/reason text for process capture, manual-only app routing, and
  driver-required virtual endpoints. Do not show an enabled action that can only
  fail.

Exit condition: a support report contains no raw PCM, credentials, or opaque
endpoint property blobs, and every unavailable feature has an actionable reason.

### Phase 3 — ACX feasibility and one virtual endpoint (external WDK gate)

Files: `drivers/windows-audio/driver/` and generated `ffi` bindings.

- Build in an eWDK environment with KMDF/ACX headers and libraries.
- Replace the bootstrap `EvtDeviceAdd` rejection with a minimal ACX adapter and
  one virtual render endpoint.
- Prove enumeration, WASAPI open, shared-mode start/stop, disable/enable, and
  clean unload on a disposable Windows VM.

Do not continue to a cable or installer milestone until this endpoint survives
that smoke test. The current SDK-only machine cannot pass this phase because it
lacks the WDK `km/crt` headers.

### Phase 4 — Virtual cable and Relay Microphone (external WDK gate)

- Add bounded render-to-capture transport in the driver with documented
  underflow/overflow behavior and no realtime allocation.
- Expose the four semantic endpoints:
  `QPWGraph Virtual Output`, `QPWGraph Virtual Monitor`, `QPWGraph Relay Sink`,
  and `QPWGraph Relay Microphone`.
- Connect the existing user-mode router/relay to those endpoints.
- Verify a deterministic tone through render → capture and peer audio through
  Relay Sink → Relay Microphone. Never change the Windows default device.

### Phase 5 — Package, security, and release gates (external)

- Generate a real `.sys`, `.inf`, `.cat`, and versioned package from eWDK.
- Run `infverif`, install/update/uninstall/reinstall, hotplug, sleep/resume,
  audio-service restart, and crash/restart tests.
- Run relevant Driver Verifier and current HLK audio tests.
- Complete Microsoft signing and Secure Boot validation before publishing a
  driver-enabled tier. Portable releases must continue to work without it.

### Phase 6 — Experimental automatic app routing (last)

- Keep `UnsupportedAppRoutePolicy` as the default.
- Only add `AudioPolicyConfig` after obtaining a verified declaration for each
  supported Windows build family; call known methods only from one isolated
  unsafe module.
- Gate it behind `experimental_app_routing = true`, report unsupported builds,
  and retain the manual Volume Mixer path.

### Phase 7 — Live acceptance and recovery

Run on a real Windows 10 build 20348+ machine and, for driver cases, a VM with
the signed package:

- deterministic tone capture, silence, process exit, child-process creation,
  1000 activation cycles, and Application Verifier;
- endpoint hotplug/profile change, default-device change, format change,
  sleep/resume, audio-service restart, and qpwgraph restart;
- application relay source disappearance/restart;
- virtual render/capture loop and Relay Microphone in OBS/Discord/browser;
- no dry+processed duplicate audio and no stale PID attachment.

## Acceptance matrix

| Scenario | Required state |
| --- | --- |
| Portable app | Starts and routes physical endpoints without the driver |
| Manual app capture | App is on `QPWGraph Virtual Output`; process-loopback source is live or clearly unavailable |
| App effects/RMS | Only qpwgraph-owned process PCM is processed and metered |
| Driver package | Four endpoints enumerate, open, stream, recover, and uninstall cleanly |
| Relay Microphone | Peer PCM reaches a normal Windows capture client |
| Saved app rule | Stable selector restores only the matching live process |
| Automatic routing | Disabled/manual by default; opt-in only after ABI conformance evidence |
| Release | Portable artifact always works; driver tier is signed and verifier/HLK-gated |

## Verification commands

Repository checks:

```powershell
cargo fmt --all -- --check
cargo test --workspace --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo check --target x86_64-pc-windows-msvc --workspace --all-features --locked
```

Driver checks:

```powershell
cargo test --manifest-path drivers/windows-audio/Cargo.toml -p qpwgraph-audio-core --locked
cargo check --manifest-path drivers/windows-audio/Cargo.toml -p qpwgraph-audio-xtask --locked
cargo make --cwd drivers/windows-audio
```

The last command requires an eWDK prompt. On an SDK-only machine it must fail
with a missing-WDK diagnostic rather than compile a user-mode substitute.

## Non-goals

ASIO, MIDI 2.0 replacement, protected-content interception, kernel-side DSP or
network relay, global Windows audio-engine replacement, registry routing hacks,
and arbitrary vtable probing are outside this plan.

## References

- [Microsoft ApplicationLoopback sample](https://github.com/microsoft/Windows-classic-samples/tree/main/Samples/ApplicationLoopback)
- [ACX overview](https://learn.microsoft.com/en-us/windows-hardware/drivers/audio/acx-audio-class-extensions-overview)
- [Windows drivers for Rust](https://github.com/microsoft/windows-drivers-rs)
- [Kernel-mode signing policy](https://learn.microsoft.com/en-us/windows-hardware/drivers/install/kernel-mode-code-signing-policy--windows-vista-and-later-)
