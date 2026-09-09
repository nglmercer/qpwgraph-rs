# Hush runtime validation

Validated 2026-09-09 against the Hush timeline and overload-recovery changes in
the qpwgraph working tree. The live probe used the physical PipeWire capture
route at 48 kHz, stereo, quantum 256.

## Implementation report

1. **Root cause:** output acceptance required callback sequence `N - 1`, so any inference longer than one host quantum was discarded forever. Once timeline scheduling was fixed, live diagnostics exposed a second defect: a slow worker could fill the input queue and keep processing audio that was already too late to play.
2. **Architecture:** input/output blocks carry stream generation, wet epoch, and absolute host-rate `start_frame`. Native 160-sample assembly, resampling, and wet output are continuous timelines. A separate wet epoch lets overload recovery reset only the wet pipeline while the aligned dry delay remains continuous.
3. **Old behavior:** a result missing its exact next callback was dropped.
4. **New behavior:** the callback requests the delayed stream range; expired ranges are discarded, future/partial ranges remain queued, and unavailable ranges use the same-position dry samples.
5. **Latency:** fixed 50 ms: 40 ms worker/frame-assembly scheduling allowance plus 10 ms Hush synthesis alignment. The 40 ms allowance is conservative against the measured release stereo worst case (6.13 ms live, 4.68 ms in the expanded benchmark) and leaves room for callback jitter. The 320-sample algorithmic metadata is not counted again.
6. **Queue:** 128 power-of-two SPSC slots per direction. Slot size is at most 10 ms, independent of PipeWire's 16,384-frame preparation ceiling.
7. **Underruns:** never block RT; retain aligned sanitized dry. Wet resumes automatically when its timeline catches up. Permanent worker failure is reported separately from temporary late output.
8. **Overflow:** input returns immediately and counts rejected blocks/frames; a partial multi-chunk enqueue requests a wet resynchronization. Output drops the newly computed range while advancing its timeline. Backlog over 80 ms also requests one resynchronization; stale input is discarded rather than sent through DeepFilterNet.
9. **Generations:** reset/reprepare, sample-rate/channel changes, connection-mask changes, and real stream gaps invalidate the stream generation. Callback quantum, jitter, parameter changes, bypass, and wet-only overload recovery do not. Wet epochs are monotonic and prevent old wet data from re-entering after recovery.
10. **Resampler delay:** centered sinc lookahead affects availability, not sample position. Impulse tests at 16/44.1/48/96 kHz stay within one host frame.
11. **Initialization/errors:** Tract remains owned by the worker because it is not `Send`; startup is synchronously acknowledged to `prepare()`. Initialization, inference, resampler, non-finite output, and panic errors retain their real reason.
12. **Realtime safety:** `process()` only sanitizes, copies, delays, uses bounded SPSC operations, and updates atomics. No allocation, inference, filesystem I/O, blocking wait, mutex lock, or join occurs there. The producer wakes the worker only when a queue changes from empty to non-empty; the worker also uses a 500 µs timed poll. Shutdown wakes/joins off RT.
13. **Bypass/reduction:** bypass is aligned delayed dry. Reduction updates use atomics and apply to the next worker chunk. Below 0.01 dB, aligned dry is selected while the worker stays warm at 0.01 dB because DeepFilterNet otherwise changes delay.
14. **Diagnostics:** exposed rate, channels, host quantum, latency, wet/dry blocks and frames, fallback reasons, underruns, queue peaks/overruns, rejected frames, stale drops, resync requests/completions, backlog, Hush frames, resets, worker average/p95/p99/max timing, readiness, overload state, and failure reason. PipeWire, Windows, and UI snapshots use the control-side diagnostic handle.
15. **Tests:** wet-vs-dry integration, direct Hush reference, 64/128/256/480/512/1024 quantums, variable quantums, artificial delay, catch-up, partial enqueue, repeated and permanently slow overload, reset-storm prevention, recovery warmup, future/partial/expired ranges, SPSC wraparound, impulse alignment, allocation-free processing, disconnect silence, non-finite input, bypass/host-disable separation, and existing adaptive/UI/backend regressions.
16. **CI:** `.github/workflows/hush.yml` now runs on relevant `main` pushes as well as pull requests and manual dispatch. The normal effects suite contains the wet-output regression.

## Measurements

Release benchmark on Linux, AMD BC-250, 12 logical CPUs. Each case ran at its
callback cadence and consumed a 100 ms tail so the output queue was drained as
a real host would do:

| rate/channels/quantum | worker avg / p95 / p99 / max (µs) | wet / dry | underruns / input / output overruns |
|---|---:|---:|---:|
| 16k/1/160 | 1,480 / 1,988 / 2,057 / 2,092 | 125 / 5 | 0 / 0 / 0 |
| 44.1k/1/441 | 1,748 / 2,365 / 2,400 / 2,408 | 125 / 5 | 0 / 0 / 0 |
| 48k/1/480 | 1,609 / 2,274 / 2,669 / 2,688 | 125 / 5 | 0 / 0 / 0 |
| 48k/2/480 | 2,903 / 3,303 / 3,581 / 4,030 | 125 / 5 | 0 / 0 / 0 |
| 48k/1/64 | 190 / 1,702 / 2,054 / 2,199 | 158 / 38 | 0 / 0 / 0 |
| 48k/1/128 | 458 / 2,078 / 2,162 / 2,178 | 139 / 19 | 0 / 0 / 0 |
| 48k/1/256 | 959 / 2,210 / 2,343 / 2,368 | 129 / 10 | 0 / 0 / 0 |
| 48k/1/512 | 856 / 1,982 / 2,374 / 2,471 | 125 / 5 | 0 / 0 / 0 |
| 48k/1/1024 | 1,241 / 2,380 / 2,573 / 2,675 | 122 / 3 | 0 / 0 / 0 |
| 48k/2/64 | 320 / 2,733 / 3,073 / 3,074 | 158 / 38 | 0 / 0 / 0 |
| 48k/2/128 | 738 / 2,976 / 3,560 / 3,648 | 139 / 19 | 0 / 0 / 0 |
| 48k/2/256 | 1,580 / 3,238 / 3,461 / 3,577 | 129 / 10 | 0 / 0 / 0 |
| 48k/2/512 | 1,572 / 3,284 / 3,928 / 4,056 | 125 / 5 | 0 / 0 / 0 |
| 48k/2/1024 | 2,150 / 3,759 / 4,531 / 4,677 | 122 / 3 | 0 / 0 / 0 |
| 96k/1/256 | 570 / 2,492 / 2,556 / 2,583 | 139 / 19 | 0 / 0 / 0 |
| 96k/1/480 | 1,111 / 2,650 / 2,775 / 2,777 | 130 / 10 | 0 / 0 / 0 |
| 96k/1/1024 | 1,164 / 2,974 / 3,063 / 3,381 | 125 / 5 | 0 / 0 / 0 |
| 96k/2/256 | 854 / 3,278 / 3,417 / 3,486 | 139 / 19 | 0 / 0 / 0 |
| 96k/2/480 | 1,738 / 3,583 / 4,579 / 4,663 | 130 / 10 | 0 / 0 / 0 |
| 96k/2/1024 | 1,924 / 4,050 / 4,643 / 4,734 | 125 / 5 | 0 / 0 / 0 |

The deterministic wet-path run reported `wet_blocks_output = 73`, `dry_fallback_blocks = 8`, `underruns = 0`, `input_overruns = 0`, and `output_overruns = 0` for 96 kHz stereo variable callbacks. The direct-adapter comparison passed within `2e-5`.

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

cargo test --release -p pw-graph-effects sustained_48k_stereo_256_keeps_wet_audio_dominant -- --ignored --nocapture

cargo run --release -p pw-graph-backend --example hush_probe --features pipewire -- --seconds 35 --reduction 40
```

`pw-graph-slint` is not a Cargo package in this checkout; its package is `pw-graph-app`, whose 141 tests passed.

## Live PipeWire check

The release `hush_probe --seconds 35 --reduction 40` was wired from the
physical stereo microphone to a null playback sink at 48 kHz/256 frames. At
the end of the run it reported:

```text
wet_blocks_output = 4,091
dry_fallback_blocks = 10
wet_frames_output = 1,047,200
dry_frames_output = 2,400
wet_ratio = 99.8%
underruns = 0
input_overruns = 0
output_overruns = 0
resync_requests/completed = 0/0
max_input_queue_depth = 2
max_output_queue_depth = 4
worker avg/p99/max = 1.79/3.87/6.13 ms
```

This is the user's failing workload and demonstrates that qpwgraph is
emitting wet Hush audio rather than continuously returning dry fallback. The
debug probe remained bounded but was overloaded by Tract scheduling: it
reported zero input/output overruns, repeated controlled resyncs, and stayed
in aligned dry recovery. That is expected for this unoptimized build; the
release path is the production performance result.

No native Windows/WASAPI machine was available for listening, and no
controlled competing-speaker/fan recording was performed. Those remain manual
validation items. Slow debug builds or sustained CPU overload use bounded,
aligned dry fallback and expose `Hush: recovering` rather than accumulating
unbounded latency.
