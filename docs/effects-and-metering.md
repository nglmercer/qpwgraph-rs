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
mono frames, so it adds an algorithmic latency of 320 samples (20 ms at its
native rate) and a 160-sample overlap-add synthesis delay. The immutable model
is embedded in the release artifact and loaded once on a setup thread.
Development and CI may override it with `QPWGRAPH_HUSH_MODEL` or `HUSH_MODEL`;
the pinned SHA-256 is recorded in `vendor/nnnoiseless/UPSTREAM.md`. Each effect
owns independent channel denoisers and a bounded worker queue. Tract inference
and resampling never run in a PipeWire or Windows realtime callback. A
delayed, sanitized dry path is used while the worker is warming up, late,
bypassed, or failed. The worker keeps atomic readiness, failure, reset, and
queue-depth and overrun diagnostics; those counters never participate in
sample generation.

The original `builtin.adaptive-noise-suppressor` remains the compatibility
implementation for saved configurations. Its persisted reduction, adaptation,
voice-preserve, and bypass parameters are not migrated to Hush. If an override
bundle is unavailable or fails checksum validation, creating a Hush effect
reports a setup error and leaves the audio path on its deterministic fallback.

Windows currently supports the built-in effect registry in the user-mode
realtime router. A persisted `module_path` is rejected explicitly because no
stable, crash-contained Windows module ABI has been released; it is not
silently ignored or loaded in the kernel driver. External module hosting is a
separate future feature with its own ABI, realtime-safety, and lifecycle gate.

Every effect host uses the same missing-input rule: an unavailable FL or FR
buffer is replaced with exact digital silence. Connected channels remain
independent, so a partial stereo route cannot contaminate its other channel.
Disabled effects, a processor lock held by a parameter update, and a processor
error or panic use a deterministic fallback of sanitized pass-through for
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
