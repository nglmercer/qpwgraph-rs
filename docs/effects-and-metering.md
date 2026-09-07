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

Windows currently supports the built-in effect registry in the user-mode
realtime router. A persisted `module_path` is rejected explicitly because no
stable, crash-contained Windows module ABI has been released; it is not
silently ignored or loaded in the kernel driver. External module hosting is a
separate future feature with its own ABI, realtime-safety, and lifecycle gate.

## Metering

Audio meters can be **Disabled**, **OnDemand**, or **Always**. On-demand helper
streams are requested only for visible PipeWire graph nodes and released when
the window is hidden or minimized. Meter requests and rendering are driven by
each node's reported capability, so meter-only and peak-only nodes are valid.
Windows uses Core Audio peak readings where available; its legacy RMS field
remains zero because Core Audio does not provide an equivalent RMS value.
**Reset audio config** releases all meter streams.

## Related

- [Adaptive noise reduction report](adaptive-noise-reduction.md) — why the
  four-band suppressor was removed and what replaced it.
- [Platform parity](platform-parity.md) — per-backend metering differences.
