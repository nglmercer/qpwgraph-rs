# QPWGraph virtual audio driver

The optional driver is isolated under `drivers/windows-audio`, a nested Cargo
workspace excluded from the portable application workspace. Its only planned
kernel responsibility is exposing four ordinary audio endpoints and moving
bounded PCM packets:

| Endpoint | Flow | Purpose |
| --- | --- | --- |
| QPWGraph Virtual Output | render | application audio sink |
| QPWGraph Virtual Monitor | capture | monitor for that sink |
| QPWGraph Relay Sink | render | receiver input to the driver |
| QPWGraph Relay Microphone | capture | microphone visible to OBS/Discord/etc. |

Endpoint ownership is not inferred from these display names. The provider must
publish the `qpwgraph_audio` service identity and the project-owned endpoint
role property (`app-render`, `app-monitor`, `relay-render`, or
`relay-capture`) on each endpoint interface; the user-mode worker records the
opaque stable endpoint id and driver version when available. The INF uses the
INF `AddProperty` sections on each audio interface for all four semantic
roles.

Mixing, effects, resampling, gain, meters, relay policy, persistence, and UI
remain in the Rust user-mode router. The stream transport is bounded and
allocation-free in the realtime path. Starvation fills capture with silence;
overflow drops the newest tail; both conditions are counted as
discontinuities.

The workspace contains a pure Rust ring core with wraparound, underflow,
overflow, and discontinuity tests. The default driver package remains a
fail-closed Stage-0 bootstrap: device-add returns `STATUS_NOT_SUPPORTED`.
An opt-in eWDK build now contains the ACX app and relay endpoint pairs,
circuit/stream callbacks, and two independent Rust-owned bounded PCM-cable
paths, but it is not installable until shared-mode, verifier, package, and
signing validation prove those endpoints on Windows. Installing the default
binary is therefore intentionally impossible rather than silently creating no
endpoint.

Build/package commands require a real eWDK prompt, KMDF 1.33, LLVM, and the
WDK tools:

```powershell
cargo test --manifest-path drivers/windows-audio/Cargo.toml -p qpwgraph-audio-core --locked
cargo make --cwd drivers/windows-audio
```

The installer must never take over Windows defaults. Development packages may
be test-signed; public Windows 10/11 packages require the Microsoft signing
pipeline, Secure Boot testing, Driver Verifier, and current HLK audio tests.
