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

Mixing, effects, resampling, gain, meters, relay policy, persistence, and UI
remain in the Rust user-mode router. The stream transport is bounded and
allocation-free in the realtime path. Starvation fills capture with silence;
overflow drops the newest tail; both conditions are counted as
discontinuities.

The workspace contains a pure Rust ring core with wraparound, underflow,
overflow, and discontinuity tests. The driver package is currently a
fail-closed Stage-0 bootstrap: its entry point returns `STATUS_NOT_SUPPORTED`
until an ACX device/circuit implementation exists. Installing that binary is
therefore intentionally impossible rather than silently creating no endpoint.

Build/package commands require a real eWDK prompt, KMDF 1.33, LLVM, and the
WDK tools:

```powershell
cargo test --manifest-path drivers/windows-audio/Cargo.toml -p qpwgraph-audio-core --locked
cargo make --cwd drivers/windows-audio
```

The installer must never take over Windows defaults. Development packages may
be test-signed; public Windows 10/11 packages require the Microsoft signing
pipeline, Secure Boot testing, Driver Verifier, and current HLK audio tests.
