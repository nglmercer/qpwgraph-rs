# Adaptive noise reduction report

## Decision

The former four-band Wiener/noise-floor implementation was removed from
`pw-graph-effects`. The existing effect ID,
`builtin.adaptive-noise-suppressor`, remains unchanged so saved graphs keep
working.

The replacement uses the native Rust `nnnoiseless` RNNoise-style model from
`vendor/nnnoiseless`. Native integration is used for PipeWire because the
reference WASM bindings expose a `wasm-bindgen` API, while this repository's
WASM effect ABI is a different host contract and currently has no WASM runtime
in the realtime PipeWire backend.

## Runtime design

- `AdaptiveNoiseSuppressor::prepare` creates the neural state, FFT plans,
  resamplers, and fixed-capacity queues.
- Input is assembled into the model's 480-sample, 48 kHz frames and converted
  back to the stream rate when necessary. Common rates from 8 kHz through 192
  kHz are supported.
- Stereo and multichannel audio are deinterleaved into independent model
  states and interleaved again without allocations in `process`.
- The realtime path only mutates prepared buffers. Queue overflow returns an
  effect error instead of allocating or panicking.

## Controls

The persisted control IDs are unchanged:

- `reduction-db` maps to the neural model's maximum attenuation floor.
- `adaptation` maps to gain-decay speed.
- `voice-preserve` maps to VAD gating and gain-release speed.
- `output-gain-db` is a separate post-denoiser gain from -12 dB to +12 dB;
  its default is 0 dB.
- `auto-gain-compensation` optionally matches the pre/post-DSP RMS level with
  a smoothed 0 dB to +6 dB correction. It is disabled by default.
- `bypass` leaves the input untouched.

Output gain is applied after denoising and is clamped to the valid `f32` audio
range. Bypass skips both fixed and automatic compensation, so enabling it does
not change the configured bypass signal level.

Parameter changes rebuild the model stream on the control thread, which resets
the denoiser history as required by the underlying model.

## Verification

The implementation was checked with:

```text
cargo fmt --all -- --check
cargo clippy -p pw-graph-effects --offline --all-targets -- -D warnings
cargo test --workspace --offline
cargo check -p nnnoiseless --no-default-features --features wasm --offline
```
