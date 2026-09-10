# Workspace architecture

How the code is split, and why the boundaries fall where they do. Start here
before adding a crate or moving logic between layers.

## Crates

The code is split into focused crates:

- `pw-graph-core`: graph models, typed durable endpoint selectors, explicit
  resolution outcomes, validation, and layout.
- `pw-graph-effects`: realtime-safe effect processor API and built-in effects.
- `pw-graph-backend`: driver abstraction, demo backend, native PipeWire graph,
  Windows Core Audio endpoint/session graph, WinMM MIDI, audio controls,
  metering, and the user-mode audio router (`router`, see
  [audio-router.md](audio-router.md)).
- `pw-graph-alsamidi`: ALSA Sequencer enumeration and routing.
- `pw-graph-command`: undoable graph commands and command history.
- `pw-graph-patchbay`: qpwgraph-compatible persistence and activation.
- `pw-graph-config`: TOML settings, compatibility preservation, and native
  platform configuration paths.
- `pw-graph-i18n`: localized message catalogs.
- `pw-graph-app-core`: framework-neutral composite application driver.
- `pw-graph-app`: canonical Slint application shell and UI bridge.
- `pw-graph-utils`: shared helpers — atomic file writes, hex, string-enum
  macros — depended on across the workspace.

Three further crates carry the relay:

- `pw-graph-relay`: the relay engine, session handling, crypto, and codecs.
- `pw-graph-relay-sdk`: the stable third-party API over that engine.
- `pw-graph-relay-android`: the JNI bindings that expose the SDK to Android.

The optional Windows driver is a separate nested workspace at
`drivers/windows-audio`; it is deliberately excluded from normal application
builds and never replaces the user-mode router.

## Layering

The `pw-graph-app-core` crate owns the framework-neutral composite backend
boundary; the canonical `pw-graph-app` bridge owns application commands,
patchbay synchronization, effects, relay, configuration, metering policy, and
persistence. The Slint shell displays that state and sends intents through the
bridge.

The practical consequence: anything that a second frontend would also need
belongs in `pw-graph-app-core` or below, and anything that only makes sense
for the Slint shell belongs in `pw-graph-app`.

## Effect lifecycle

The effect SDK separates persisted intent, provider metadata, preparation, and
realtime processing. `ChannelPolicy` records `Auto` or an explicit fixed
layout; backend negotiation produces the live `AudioSpec` without rewriting
that policy. `EffectProvider` owns effect-specific construction and can
override its non-realtime `prepare_instance` hook for model or module-backed
resources. `EffectComponentManager` runs those hooks on a small bounded loader
pool and emits ticketed progress/ready/failure events.

PipeWire activates a prepared processor only after `Ready`, then publishes the
filter and performs any link replacement. `EffectProcessor::process` remains a
synchronous, preallocated realtime interface. Model loading, module
initialization, filesystem access, waits, and worker joins stay outside the
audio callback; Hush additionally sends deferred worker joins to a bounded
reaper.

## Backend namespacing

Graph IDs use explicit backend namespaces, so each native driver receives only
resources it owns. Linux PipeWire/ALSA routing and Windows WinMM MIDI links are
mutable; Windows Core Audio endpoint/session relationships are observed, so
their connection, disconnection, and rerouting requests report unsupported.
WinMM device indices are used only for the current native open; stable device
interface identities keep graph IDs from following enumeration order changes.
Windows virtual endpoint names are classified into semantic roles, and
persisted application routes use process selectors rather than transient PIDs.

## Assets

Shared SVG assets live in [`assets/icons`](../assets/icons).

## Durable graph identity and reconciliation

PipeWire registry records capture node properties and join parent Client
metadata before building graph nodes. `NodeIdentity` deliberately separates
durable application hints (`application.id`, process binary/name, effect
instance) from current-session diagnostics (`object.serial`, client/global
IDs). `EndpointSelector` and `EndpointResolution` live in `pw-graph-core`, so
matching never depends on Slint or on a numeric-ID tie breaker. The resolver
returns `Exact`, `UniqueFallback`, `Ambiguous`, or `Missing`; equal candidates
are therefore safe to inspect rather than unsafe to guess.

`pw-graph-patchbay` owns schema-v2 selectors, legacy migration, qpwgraph XML
compatibility, and the control-plane `PatchbayReconciler`. An activated
patchbay marks itself dirty on registry changes, waits for a short settle
window, resolves all desired routes, and then performs only idempotent missing
connects (or safe exclusive cleanup). Missing dynamic applications remain
pending, ambiguous matches remain untouched, and backend failures retry with a
bounded backoff. Manual deletion updates desired state so the reconciler does
not fight the user.

The Slint bridge requests compact Hush snapshots during ordinary synchronization
and asks the effect driver for a full report only for the diagnostics dialog.
The same copyable dialog pattern is used for PipeWire identity and patchbay
reconciliation reports. None of these control-plane operations enter the Hush
realtime callback; model loading, inference, filesystem I/O, locks, waits, and
joins remain worker/control-plane work.

## Further reading

- [Slint UI structure](ui-components.md) — how `src/bridge/` translates
  application models into Slint rows.
- [Platform parity](platform-parity.md) — what each backend can and cannot do.
