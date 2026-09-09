# Hush runtime validation

Validated 2026-09-09 against audited commit `4fff0824393dc703ff6def246b7406b64da47d59`.

## Implementation report

1. **Root cause:** output acceptance required callback sequence `N - 1`, so any inference longer than one host quantum was discarded forever. Callback-size changes also reset the stream, and incomplete worker frames were emitted as callback-sized zero-padded blocks.
2. **Architecture:** input/output blocks now carry `generation` and absolute host-rate `start_frame`. Native 160-sample assembly, resampling, and wet output are continuous timelines.
3. **Old behavior:** a result missing its exact next callback was dropped.
4. **New behavior:** the callback requests the delayed stream range; expired ranges are discarded, future/partial ranges remain queued, and unavailable ranges use the same-position dry samples.
5. **Latency:** fixed 50 ms: 40 ms worker/frame-assembly scheduling allowance plus 10 ms Hush synthesis alignment. The 320-sample algorithmic metadata is not counted again.
6. **Queue:** 128 power-of-two SPSC slots per direction. Slot size is at most 10 ms, independent of PipeWire's 16,384-frame preparation ceiling.
7. **Underruns:** never block RT; retain aligned sanitized dry. Wet resumes automatically when its timeline catches up.
8. **Overflow:** input returns immediately and counts an overrun; output drops the newly computed range while advancing its timeline. Input older than 100 ms is skipped and the worker re-syncs.
9. **Generations:** reset/reprepare, sample-rate/channel changes, connection-mask changes, and real stream gaps invalidate state. Callback quantum, jitter, parameter changes, and bypass do not.
10. **Resampler delay:** centered sinc lookahead affects availability, not sample position. Impulse tests at 16/44.1/48/96 kHz stay within one host frame.
11. **Initialization/errors:** Tract remains owned by the worker because it is not `Send`; startup is synchronously acknowledged to `prepare()`. Initialization, inference, resampler, non-finite output, and panic errors retain their real reason.
12. **Realtime safety:** `process()` only sanitizes, copies, delays, uses bounded SPSC operations, and updates atomics. No allocation, inference, filesystem I/O, blocking wait, mutex lock, or join occurs there. Worker wake-up uses a 500 µs timed poll; shutdown wakes/joins off RT.
13. **Bypass/reduction:** bypass is aligned delayed dry. Reduction updates use atomics and apply to the next worker chunk. Below 0.01 dB, aligned dry is selected while the worker stays warm at 0.01 dB because DeepFilterNet otherwise changes delay.
14. **Diagnostics:** exposed rate, channels, host quantum, latency, wet blocks, dry blocks, underruns, queue peaks, overruns, resets, worker average/p99/max timing, readiness, and failure reason. PipeWire, Windows, and UI snapshots use the control-side diagnostic handle.
15. **Tests:** wet-vs-dry integration, direct Hush reference, 64/128/256/480/512/1024 quantums, variable quantums, artificial delay, catch-up, overflow, reset-storm prevention, recovery warmup, future/partial/expired ranges, SPSC wraparound, impulse alignment, allocation-free processing, disconnect silence, non-finite input, and existing adaptive/UI/backend regressions.
16. **CI:** `.github/workflows/hush.yml` now runs on relevant `main` pushes as well as pull requests and manual dispatch. The normal effects suite contains the wet-output regression.

## Measurements

Release benchmark on Linux, AMD BC-250, 12 logical CPUs:

| rate/channels/quantum | worker avg / p95 / p99 / max (µs) | wet / dry | underruns / input / output overruns |
|---|---:|---:|---:|
| 16k/1/160 | 1,379 / 1,639 / 1,820 / 1,874 | 115 / 5 | 0 / 0 / 0 |
| 44.1k/1/441 | 1,561 / 1,821 / 1,958 / 1,992 | 115 / 5 | 0 / 0 / 0 |
| 48k/1/480 | 1,500 / 1,796 / 1,978 / 2,068 | 115 / 5 | 0 / 0 / 0 |
| 48k/1/64 | 201 / 1,517 / 1,582 / 1,596 | 83 / 38 | 0 / 0 / 0 |
| 48k/1/128 | 415 / 1,574 / 1,866 / 1,986 | 102 / 19 | 0 / 0 / 0 |
| 48k/1/256 | 852 / 1,751 / 1,837 / 1,855 | 111 / 10 | 0 / 0 / 0 |
| 48k/1/512 | 851 / 1,868 / 2,109 / 2,144 | 116 / 5 | 0 / 0 / 0 |
| 48k/1/1024 | 1,135 / 2,048 / 2,326 / 2,459 | 118 / 3 | 0 / 0 / 0 |
| 48k/2/480 | 2,979 / 3,264 / 3,377 / 3,780 | 115 / 5 | 0 / 0 / 0 |
| 96k/1/960 | 2,169 / 3,299 / 4,185 / 4,373 | 115 / 5 | 0 / 0 / 0 |
| 96k/2/960 | 3,382 / 3,770 / 3,870 / 3,889 | 115 / 5 | 0 / 0 / 0 |

The required wet-path run reported `wet_blocks_output = 73`, `dry_fallback_blocks = 8`, `underruns = 0`, `input_overruns = 0`, and `output_overruns = 0` for 96 kHz stereo variable callbacks. The direct-adapter comparison passed within `2e-5`.

## Validation commands

Passed:

```text
cargo fmt --all -- --check
cargo check --workspace --all-features
cargo test --workspace --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test -p pw-graph-effects --all-features
cargo test -p pw-graph-backend --all-features
cargo test -p pw-graph-app --all-features
HUSH_MODEL=$PWD/crates/pw-graph-effects/resources/hush/advanced_dfnet16k_model_best_onnx.tar.gz cargo test -p nnnoiseless --features hush --test hush --test hush_buffer -- --nocapture
cargo check -p pw-graph-backend --all-features --target x86_64-pc-windows-gnu
cargo test -p pw-graph-effects benchmark_hush_rates -- --ignored --nocapture
cargo test --release -p pw-graph-effects benchmark_hush_rates -- --ignored --nocapture
```

`pw-graph-slint` is not a Cargo package in this checkout; its package is `pw-graph-app`, whose 141 tests passed.

## Live PipeWire check

A release `hush_probe` was wired from the physical stereo microphone to a four-channel recorder at 48 kHz/128 frames. Direct microphone and Hush output were recorded simultaneously. Reduction was swept through 0/20/40/60 dB; both Hush inputs were disconnected for two seconds and reconnected. Processed channels changed level with reduction, were exact zero while disconnected, and resumed after reconnect. A representative clean run reached `wet_blocks_output = 5,424`, `dry_fallback_blocks = 15`, `underruns = 0`, `input_overruns = 0`, `output_overruns = 0`, with worker average 0.95 ms, p99 5.89 ms, max 19.76 ms. The recording's direct-vs-wet correlation peaked near 42.7 ms, consistent with fixed scheduling plus model alignment.

No native Windows/WASAPI machine was available for listening, and no controlled competing-speaker/fan test was performed. Those remain manual validation items. Slow debug builds or sustained CPU overload may legitimately use aligned dry fallback; release measurements and live PipeWire output demonstrate that Hush wet audio reaches qpwgraph output.
