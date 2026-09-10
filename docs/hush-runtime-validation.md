# Hush runtime validation

Validated in the working tree based on commit `21d2584` plus the asynchronous
effect-lifecycle, durable identity, reconciliation, and Hush optimization
changes on 2026-09-09.

## Root cause

The primary routing bug was earlier than inference: standalone Hush creation
treated `channels = None` as the legacy two-channel DSP layout. A mono
microphone therefore constructed two independent denoisers, while link
insertion used a separate topology inference path. The resolved layout was
also not persisted, so a restored standalone mono effect could become stereo
again.

The runtime had a second, independent failure mode: when inference missed its
deadline, the aligned dry fallback made an enabled effect sound bypassed. The
worker was slower than realtime in the reported debug/live configuration, and
its rolling realtime-factor window was erased by every wet-pipeline reset. A
slow worker therefore kept restarting before it could become confidently
classified as permanently slow.

The fix uses one generic provider-owned channel negotiation path: explicit
`ChannelPolicy::Fixed(1/2)` requests win, insertion derives 1/2 from source
topology, and unresolved standalone `ChannelPolicy::Auto` safely starts mono.
The policy is persisted separately from the runtime resolution, so restoring a
mono first-run `Auto` node does not silently make it permanently fixed mono.
The worker also separates mutable Hush DSP state from persistent performance
state. A wet reset clears denoisers, resamplers, pending buffers, warmup, and
the wet epoch, but retains lifetime timing, EWMA service rate, high-percentile
timing, and overload confidence.

Health is a separate control-plane calculation. The realtime callback only
increments cumulative per-reason frame atomics (`startup`, manual bypass, host
disabled, underrun, worker failure, resync, and no input). The UI/control side
samples those counters into a bounded recent window, so deliberate dry output
from an earlier bypass period does not poison later health. Worker failure and
CPU overload still take precedence over delivery percentages.

Effect creation now has two phases. `EffectComponentManager::begin_prepare`
returns an `EffectTicket` without waiting; its bounded loader invokes the
provider preparation hook. Hush loads the shared model and starts its
Tract-owning worker there, waits for the worker's readiness only on that loader
thread, and reports a failed initialization as a failed ticket. PipeWire
publication and link replacement happen only after `Ready`, leaving the
original route intact when preparation fails. Destruction uses a bounded Hush
reaper rather than joining a worker from the UI/control path.

## Runtime policy

- A complete 50 ms performance interval updates the lifetime factor and a
  persistent EWMA. The EWMA smoothing coefficient is 0.75, so a cold sample
  is visible without immediately changing state.
- Overload entry requires raw and EWMA realtime factor above `1.10` for three
  observations plus real backlog. Exit requires both below `0.90` for three
  observations, fresh wet lead, and a bounded post-recovery backlog.
- `Overloaded` stops accepting normal input, drains stale work, keeps the
  aligned dry timeline running, and does not reset Hush on every callback.
  The overload cause is reported as CPU throughput, backlog, or queue full.
- Automatic retry cooldowns are 500 ms, 1 s, 2 s, then 5 s. Automatic retries
  stop after three attempts; an explicit control-path retry remains available
  after re-enable, layout, sample-rate, or generation changes.
- The wet request is computed from an absolute callback start and is asserted
  to end no later than that start. `latest_submitted_frame`,
  `latest_completed_input_frame`, and `latest_wet_frame` are published
  explicitly.
- Scheduling starts at 40 ms, is always at least the active host quantum plus
  minimum headroom, and only grows during a prepared stream. At 48 kHz it is
  1,920 frames for quantum 512 and 2,528 frames for quantum 2,048. With the
  480-frame Hush synthesis delay, those total latencies are 50.0 ms and
  62.7 ms.
- PipeWire creates mono Hush ports and one denoiser for mono routes. Full
  stereo routes use one independent-mask multi-channel DfTract runtime;
  partial routes fall back to only the connected mono runtimes. A disconnected
  channel is not inferred as audio and does not run inference; its output is
  exact zero.
  `ChannelPolicy` is persisted; only legacy numeric `channels` values become
  `Fixed(n)`. Legacy configurations without a hint use `Auto`, which starts
  mono for an unresolved standalone node and uses topology inference for link
  insertion.

## Measurements

The reported failing session showed approximately `1.99x` realtime and
`25.8 ms` worker p95/p99 at a `10.67 ms` 48 kHz/512 callback budget, with
`active channels: 2` and nearly all output dry. The optimized build on this
machine is materially different: direct native Hush is about `0.112–0.117x`
mono and `0.229–0.237x` for two independent channels, so the model itself is
not the source of that 2x result.

The qpwgraph benchmark uses the complete resampling, worker, queue, and wet
timeline path. Its wet ratio includes the initial 50 ms dry startup period;
the sustained 30-second release test measures the post-startup health.

The most recent release measurements are below. Values vary with host load, so
these are regression baselines rather than hard requirements.

| measurement | before | after |
|---|---:|---:|
| integrated 48 kHz / stereo / quantum 256 lifetime RT, EWMA RT | 0.339x, 0.314x | 0.185x, 0.180x |
| integrated 48 kHz / stereo / quantum 256 worker p95 / p99 | 4.543 / 6.324 ms | 2.395 / 2.431 ms |
| sustained 48 kHz / stereo / quantum 256 wet ratio | 0.983 (36 underruns, 1 resync) | 0.992 (9 underruns, 1 resync) |
| stereo Hush inference, 3.2 s: two mono runtimes → one independent-mask runtime | 831.87 → 515.67 ms; init 249.61 → 125.67 ms | max output difference `0.000000` |
| stereo 48↔16 resampling, 4 s: two mono → shared generic → fixed polyphase | 216.392 → 126.548 ms | 77.404 ms; max difference `0.000000` |

| build | rate / channels / quantum | worker avg / p95 / p99 / max | lifetime / EWMA RT | wet ratio | queue overruns / resync |
|---|---|---:|---:|---:|---:|
| debug (reported before fix) | 48k / 1 / 512 | 4.726 / 9.684 / 9.782 / 9.929 ms | 0.886 / 0.808x | 96.4% | 0 / 0 |
| debug (reported before fix) | 48k / 2 / 512 | 9.708 / 18.869 / 18.869 / 18.869 ms | 1.731 / 1.739x | 0.8% | 0 / 1 |
| debug (reported before fix) | 48k / 1 / 2,048 | 7.871 / 9.956 / 11.425 / 13.068 ms | 0.922 / 0.808x | 95.7% | 0 / 0 |
| debug (reported before fix) | 48k / 2 / 2,048 | 15.135 / 18.503 / 18.503 / 18.503 ms | 1.724 / 1.727x | 0.3% | 0 / 1 |
| release | 48k / 1 / 512 | 0.867 / 2.043 / 2.330 / 2.892 ms | 0.163 / 0.153x | 96.4% | 0 / 0 |
| release | 48k / 2 / 512 | 1.582 / 3.371 / 3.830 / 4.117 ms | 0.297 / 0.275x | 96.4% | 0 / 0 |
| release | 48k / 1 / 2,048 | 1.279 / 1.919 / 2.133 / 2.801 ms | 0.150 / 0.132x | 98.3% | 0 / 0 |
| release | 48k / 2 / 2,048 | 2.512 / 3.630 / 4.138 / 5.681 ms | 0.294 / 0.259x | 98.3% | 0 / 0 |

In that earlier expanded release run, input pushes were 258 for quantum 512 and 610 for
quantum 2,048. All four headline cases had zero input blocks rejected, zero
intentional-overload skips, zero queue overruns, and zero stale wet drops;
active Hush channels were 1/2 for mono/stereo respectively. The 2,048-frame
cases recorded 24 partial-output underrun callbacks under that host's test
pacing, while retaining 98.3% wet frames. The current optimized 256-frame
result is reported in the before/after table above.

The current sustained 48 kHz/stereo/256 release test produced 1,428,128 wet
frames and 11,872 dry frames: 99.2% wet, zero queue overruns, nine transient
underrun callbacks, and one completed resync. It passes the post-startup
dominance threshold; the remaining resync is deliberately visible rather than
hidden by a lifetime health ratio.

The direct native 16 kHz benchmark processed 320 native frames per channel,
with three attenuation values and four block sizes. Across those cases:

| direct path | avg / p95 / p99 / max | RT factor |
|---|---:|---:|
| Hush mono | 1.086–1.149 / 1.116–1.191 / 1.142–1.356 / 1.197–1.444 ms | 0.109–0.115x |
| Hush, two independent channels | 1.109–1.120 / 1.150–1.179 / 1.174–1.253 / 1.245–1.378 ms | 0.222–0.224x |

CPU percentage was not instrumented by the benchmark; timing and realtime
factor are the authoritative measurements currently available.

The portable release profile remains unchanged. As opt-in experiments, the
native-target run measured about 0.109–0.110x mono and 0.222–0.223x stereo;
thin LTO with one codegen unit measured about 0.109–0.111x mono and
0.222–0.223x stereo. Neither is enabled by default, and the Hush scheduling
latency remains 50 ms (62.7 ms at the 2,048-frame host quantum); no latency
reduction was promoted without a broader safety-margin study.

## Validation

Passed in this checkout:

```text
cargo fmt --all -- --check
cargo check --workspace --all-features
cargo test --workspace --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo build --release
HUSH_MODEL=../../crates/pw-graph-effects/resources/hush/advanced_dfnet16k_model_best_onnx.tar.gz cargo test -p nnnoiseless --features hush --test hush --test hush_buffer -- --nocapture
cargo test --release -p pw-graph-effects benchmark_direct_hush_rates -- --ignored --nocapture
cargo test --release -p pw-graph-effects benchmark_hush_rates -- --ignored --nocapture
cargo test --release -p pw-graph-effects benchmark_hush_multichannel_variants -- --ignored --nocapture
cargo test --release -p pw-graph-effects benchmark_stereo_resampler_variants -- --ignored --nocapture
cargo test --release -p pw-graph-effects sustained_48k_stereo_256_keeps_wet_audio_dominant -- --ignored --nocapture
cargo test -p pw-graph-backend --lib --features pipewire
cargo test -p pw-graph-config --release
```

The model-test path is relative to Cargo's package working directory; using
an absolute `HUSH_MODEL` path is also valid.

The live release probe was run with `--channels 2`. It successfully loaded
and published the Hush node, but this environment had no probe links, so
PipeWire delivered zero callbacks (`quantum 0`, wet 0, processed blocks 0).
The final cleanup round-trip returned a PipeWire `unknown resource` error;
this is a daemon/registry teardown issue, not an Hush inference result. A
connected microphone-to-sink probe remains required on a real PipeWire graph.

The read-only PipeWire property inspection found the expected compatibility
case for Discord: the live `WEBRTC VoiceEngine` nodes exposed
`application.name=WEBRTC VoiceEngine` and `application.process.binary=Discord`,
while `application.id` was absent. `client.id`, `object.serial`, and global
node/port IDs were present but session-local. The associated Client records
carried the useful application/process metadata; media names were
`recStream`/`playStream`. This validates using Client inheritance and process
fallbacks, not treating `application.id` as universally available. A live
destroy/recreate was not forced because it would disrupt the active Discord
session; deterministic generation-churn tests cover the route restoration
state machine.

`cargo check -p pw-graph-backend --all-features --target x86_64-pc-windows-gnu`
passed. No Windows/WASAPI listening machine was available, so no Windows
runtime result is claimed here.

The legacy `builtin.adaptive-noise-suppressor` path and existing UI selection,
typed callbacks, persistence, bypass, silence, and monitoring-safety tests
remain covered by the workspace suite.
