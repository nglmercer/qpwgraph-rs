# Workspace architecture

How the code is split, and why the boundaries fall where they do. Start here
before adding a crate or moving logic between layers.

## Crates

The code is split into focused crates:

- `pw-graph-core`: graph models, stable endpoint keys, validation, and layout.
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

## Further reading

- [Slint UI structure](ui-components.md) — how `src/bridge/` translates
  application models into Slint rows.
- [Platform parity](platform-parity.md) — what each backend can and cannot do.
