# Features

What qpwgraph-rs does today, across both backends. Where a capability differs
between Linux and Windows, the difference is stated here and detailed in
[Platform parity](platform-parity.md).

## Graph and routing

- PipeWire and ALSA MIDI graphs in one view on Linux, with invalid
  cross-backend links rejected.
- Windows playback/capture endpoints and active application audio sessions,
  with endpoint/session volume, mute, and native peak metering where exposed.
- Native Windows Core Audio notifications for endpoint and session changes.
- Windows WinMM MIDI devices with stable interface-based identities and real
  input-to-output routing.
- Windows Core Audio graph relationships are informational: arbitrary
  system-wide audio routing is not exposed as a mutable patchbay. A session
  already assigned to QPWGraph Virtual Output can opt into process-loopback
  routing through the user-mode router.

## Editing

- Easy grouped-channel and Advanced individual-port connection modes.
- Multi-selection, box selection, node movement, arrange, minimap, thumbnails,
  search, media filters, overlap avoidance, and connect-through behavior.
- Undo, redo, command history, and atomic grouped graph operations.

## Persistence

- qpwgraph-compatible `.qpwgraph`, `.xml`, and JSON patchbay files, including
  autosave, profiles, recent files, rules, activation, and stable endpoint
  restoration.
- Node names, colors, collapsed state, positions, preferences, and effect
  instances persisted in the shared application configuration.

See [Configuration and patchbay files](configuration.md).

## Processing and monitoring

- Built-in effect gallery with routed insertion, standalone nodes, every
  parameter, bypass, restoration, and cleanup.
- Disabled, on-demand, and always-on audio metering.
- Windows process-loopback PCM sources provide per-application true RMS and
  effects once the application is isolated on QPWGraph Virtual Output.

See [Effects and metering](effects-and-metering.md).

## Relay

- Optional relay host, discovery, client sessions, QR pairing, and virtual
  relay nodes. With the optional Windows driver, received audio can be exposed
  as QPWGraph Relay Microphone; without it, direct physical output remains
  available.

See [Audio relay](audio-relay.md) and the
[relay wire protocol](relay-protocol.md).

## Platform integration

- English, Spanish, and French localization.
- Tray integration on Linux and Windows, start-minimized mode, native file
  dialogs, Nix, Flatpak, AppImage, and portable Windows ZIP packaging.

See [Packaging and releases](packaging.md).

## One canonical binary

The production UI is Slint and the canonical executable is always
`qpwgraph-rs`.
