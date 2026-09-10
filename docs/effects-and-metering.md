# Effects and metering

The processing gallery and the audio meters, including what each costs when
it is left on.

## Effects

Open **Effects** to create a standalone processing node or insert an effect
into a selected audio link. Effect parameters, bypass state, stable routing,
positions, and restoration are persisted. Startup restores standalone effects,
activates the patchbay when configured, and then restores routed effects.

That order matters: a routed effect needs the link it sits on to exist before
it can be reinserted, so patchbay activation has to run in between.

New noise-suppression effects use the separate `builtin.hush-noise-suppressor`
descriptor. Hush runs the pinned DeepFilterNet-SE model at 16 kHz in 160-sample
frames. Full stereo uses one independent-mask multi-channel runtime; partial
stereo and mono routes use only the connected mono runtimes. Its 320-sample
algorithmic latency metadata and 160-sample
streaming overlap-add synthesis delay describe different aspects of the model;
they are **not additive**. qpwgraph schedules the raw streaming output 40 ms
later and aligns the dry path by another 10 ms of synthesis delay: **50 ms total**
(800 / 2,205 / 2,400 / 4,800 frames at 16 / 44.1 / 48 / 96 kHz). The 40 ms
scheduling allowance is a conservative margin over the measured release worker
maximum (5.68 ms in the final expanded benchmark and 6.13 ms in the live 48 kHz
stereo probe), rather than a callback-count assumption.

The immutable model is embedded and checksum verified. Development and CI may
override it with `QPWGRAPH_HUSH_MODEL` (preferred) or `HUSH_MODEL`; the winning
source, path, byte counts, and checksum are reported, and an invalid override
does not silently fall back to the embedded model. Effect creation is split
into a bounded preparation phase and a PipeWire activation phase: the UI gets
an effect ticket immediately, model/denoiser setup runs on a loader worker,
and the node is published only after a `Ready` event. `HushRuntime::spawn()`
does not wait on the denoiser thread; the loader waits for readiness off the
UI/control and realtime threads and forwards initialization errors visibly.
Tract state is not `Send`, so construction and ownership stay on the worker;
no unsafe cross-thread transfer is used.

Input and output carry generation and absolute host-frame positions. Native
frame assembly and both sinc resamplers remain continuous across callbacks.
Callbacks read overlapping output ranges, preserve future/partial blocks, and
discard only expired ranges or obsolete generations. Missing ranges retain
sanitized dry samples from the same fixed timeline. Bypass, including host
Disable, continues running the stream and selects aligned dry output.

Both queues have 128 slots, each at most 10 ms (or the smaller prepared maximum).
Larger host callbacks are split during bounded copying. At 48 kHz stereo the two
queues reserve about 960 KiB of sample storage, even with a 16,384-frame host
capacity ceiling. Input overruns never wait. Output overruns drop the newly
computed range but advance its timeline, so subsequent wet samples cannot move
in time. The worker tracks absolute input, playout, and wet positions instead of
inferring load from queue depth. More than 80 ms of useful backlog, a partial
enqueue, or an irrecoverably late wet range requests one wet-pipeline
resynchronization. The worker drains stale input/output, advances a wet epoch,
resets Hush and both resamplers, and adopts a recent live origin. Old-epoch
blocks are dropped without triggering another reset. The aligned dry delay
remains continuous across this wet-only recovery. A 40 ms recovery lead is
required before the worker leaves its recovering state. The first synthesis-delay
samples after a fresh origin are not published as wet; aligned dry remains in
place until output represents valid input from that origin.

Reset/reprepare and connection-mask changes invalidate the generation. Callback
size changes, reduction changes, bypass, and ordinary worker jitter do not.
Reduction updates apply when the worker takes its next input chunk, using
`set_attenuation_limit_db`, without rebuilding the model. Below 0.01 dB, the effect selects aligned dry
audio while running the worker at 0.01 dB: DeepFilterNet otherwise short-circuits
synthesis and changes the signal delay. Reconnection warms a
clean recurrent/resampler state. Disconnected channels are zeroed immediately,
even if old wet audio remains queued.

The generic sinc filters center their first output on source position zero and
remain the fallback for nonstandard rates. Exact 48 kHz ↔ 16 kHz Hush streams
use a precomputed 3:1/1:3 polyphase converter with circular history; its
streaming latency and alignment are covered by the same impulse and length
tests. Both paths need roughly 2.2 ms of combined lookahead at the tested
rates. That lookahead and native-frame assembly consume part of the 40 ms
scheduling allowance; neither is added again to the dry alignment. See
[runtime validation](hush-runtime-validation.md) for measurements and the
limits of the live tests.

`process()` only sanitizes/copies samples, advances preallocated delay storage,
uses bounded SPSC operations, and updates atomics. It neither allocates nor
locks, waits, joins, loads a model, or invokes Tract. The producer notifies the
worker only when a queue transitions from empty to non-empty; the worker also
uses a 500 µs timed wait. Expensive resets and shutdown joins stay outside
realtime processing. Hush worker joins are handed to one bounded reaper, so
removing an effect does not make the UI or callback wait for an in-flight
inference.

PipeWire Hush parameter updates use shared atomics rather than the generic
processor mutex, so slider and bypass changes do not introduce an instantaneous
host fallback. The existing typed events and stable parameter models are retained.

Noise-suppression effects expose `Output Gain` (`output-gain-db`, -12 dB to
+12 dB, default 0 dB) as a post-denoiser stage. `Automatic Gain Compensation`
(`auto-gain-compensation`) is off by default; when enabled it compares input
and processed RMS levels, adds at most +6 dB, and smooths changes to avoid
pumping. Both gain paths are skipped while the effect is bypassed, and the
post-gain samples are clamped to -1.0..=1.0.

The normal Effects card uses a cheap typed summary: health, rate, channel
layout, realtime factor, recent wet percentage, and the most useful worker
timing. It does not rebuild the large diagnostics report on every UI tick.
Health is computed from a bounded recent control-plane window (currently about
three seconds), with dry frames tracked by reason. Historical startup,
intentional bypass, and host-disabled frames therefore do not permanently make
a later healthy stream appear degraded; underruns, no-input periods, overload,
and worker failure remain visible and distinct. Timing is **worker input-chunk
time**, including conversion and any inference; small chunks do not all run
inference. Percentiles use a bounded 4,096-observation window. Wet and dry
block counters can both increment for a callback containing a partial wet
range.

Open **Debug** on an effect to request the full report on demand. The
diagnostics dialog includes the raw selectable report and a cross-platform
native copy action. Effect creation is also a background operation: closing
Effects discards only the unsubmitted draft, while an already-ticketed model
load continues in the background. Its operation card exposes explicit Cancel;
completion becomes a toast and failure remains inspectable without requiring
the modal to stay open.

For all effects, `ChannelPolicy::Auto` and `ChannelPolicy::Fixed(n)` are kept
separate from the negotiated runtime width. Legacy `channels = 1/2` values
migrate to `Fixed(1/2)`; an absent legacy value migrates to `Auto`. For Hush,
insertion derives the source layout, while an unresolved standalone node
safely starts mono. The policy, rather than that first runtime resolution, is
persisted, so restoration does not accidentally turn `Auto` into `Fixed(1)`.
The original
`builtin.adaptive-noise-suppressor` remains the compatibility
implementation for saved configurations. Its persisted reduction, adaptation,
voice-preserve, and bypass parameters are not migrated to Hush. If an override
bundle is unavailable, cannot be parsed, or cannot initialize a compatible
runtime, creating a Hush effect reports a setup error and does not create a
fake working node.

Windows currently supports the built-in effect registry in the user-mode
realtime router. A persisted `module_path` is rejected explicitly because no
stable, crash-contained Windows module ABI has been released; it is not
silently ignored or loaded in the kernel driver. External module hosting is a
separate future feature with its own ABI, realtime-safety, and lifecycle gate.

Every effect host uses the same missing-input rule: an unavailable FL or FR
buffer is replaced with exact digital silence. Connected channels remain
independent, so a partial stereo route cannot contaminate its other channel.
Except for Hush's aligned host bypass, disabled effects, a processor lock held
by a parameter update, and a processor error or panic use a deterministic fallback of sanitized pass-through for
connected channels and silence for missing channels. The callback publishes
failure or dangling-input state separately; it never emits an audible error
indicator or leaves an output buffer undefined.

## Metering

Audio meters can be **Disabled**, **OnDemand**, or **Always**. On-demand helper
streams are requested only for visible PipeWire graph nodes and released when
the window is hidden or minimized. Meter requests and rendering are driven by
each node's reported capability, so meter-only and peak-only nodes are valid.
Windows uses Core Audio peak readings where available; its legacy RMS field
remains zero because Core Audio does not provide an equivalent RMS value.
**Reset audio config** releases all meter streams.

PipeWire meters are passive, hidden capture helpers (`stream.monitor=true`,
`node.passive=true`, and an explicit target). They only observe the target and
do not participate in routing or DSP decisions. In particular, attaching a
meter can activate an effect output callback, but an effect with no real input
still writes silence. Removing a meter, or waiting through the linger period,
therefore cannot change the target's audio or leave generated sound behind.

## Related

- [Adaptive noise reduction report](adaptive-noise-reduction.md) — why the
  four-band suppressor was removed and what replaced it.
- [Platform parity](platform-parity.md) — per-backend metering differences.
