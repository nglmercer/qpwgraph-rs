# qpwgraph-rs

Rust/Slint patchbay for PipeWire, with optional ALSA Sequencer MIDI support.

https://github.com/user-attachments/assets/d7a9b1d4-d6d3-4ef2-b0d1-4cfc2de64650

## Workspace

The code is split into small crates:

- `pw-graph-core`: serializable nodes, ports, links, validation, and layout.
- `pw-graph-effects`: realtime-safe effect processor API, built-in noise gate,
  and the versioned WASM module ABI.
- `pw-graph-backend`: the driver abstraction, deterministic Demo backend,
  native PipeWire registry/link backend, and optional audio meters.
- `pw-graph-alsamidi`: native ALSA Sequencer enumeration and routing.
- `pw-graph-command`: undoable connect, grouped connect, disconnect,
  disconnect-all, and node-layout commands.
- `pw-graph-patchbay`: qpwgraph-compatible XML and JSON persistence/activation.
- `pw-graph-config`: TOML settings and XDG paths.
- `pw-graph-i18n`: English, Spanish, and French catalogs with English fallback.
- `pw-graph-ui`: framework-neutral graph projection, selection state, ID
  translation, filtering, meters, and Easy/Advanced connection semantics.
- `pw-graph-app`: Slint desktop shell, the pinned `slint-node-editor` graph,
  backend composition, tray, relay, and CLI.

## Interface

The application uses one Slint window with a toolbar, graph canvas, status bar,
and modal overlays. The graph is rendered by the pinned
[`slint-node-editor`](https://github.com/tilladam/slint-node-editor) component;
the application owns the PipeWire model and projects it into Slint rows.
Refresh, undo/redo, Easy/Advanced connect mode, layout, filters, patchbay
actions, relay, effects, and **Disconnect all** are available from the
toolbar or overlays. The node editor supplies selection, box selection,
drag-to-connect, node dragging, minimap, pan/zoom, and curved links.
Disconnect all is one undoable command and removes live connections from the
saved patchbay rules just like an individual disconnect.

The graph refreshes automatically when PipeWire registry events arrive. The
search field filters nodes and ports by name. Dragging an Easy-mode group or a
node creates one undoable transaction.

Easy mode groups compatible audio channels. PipeWire ports use the backend's
`audio.channel` metadata when available; demo and ALSA ports use a conservative
name-suffix fallback. Advanced mode always renders one row per port.

Node names are displayed using read-only aliases. Native PipeWire rename is not
exposed because client-owned names cannot be changed safely by the graph UI.

## Run

```bash
cargo run -p pw-graph-app
cargo run -p pw-graph-app -- --demo
cargo run -p pw-graph-app -- --lang es
```

`--demo` starts the deterministic demo graph. Without it, a missing live
backend produces an empty graph and an explanatory status message.

## Nix

The flake provides a native Linux package and a development shell with the
PipeWire, ALSA, GTK, Wayland, X11, and Opus dependencies used by the desktop
application:

```bash
nix develop
nix build
nix run
nix flake check
```

The default package builds `pw-graph-app` with its default native features and
runs the workspace tests with all features enabled.

With [direnv](https://direnv.net/) installed, allow the repository once to
load the same development shell automatically:

```bash
direnv allow
```

## Releases

Releases are published by GitHub Actions from tags matching `vX.Y.Z`. The tag
must match the workspace version in `Cargo.toml`; prerelease suffixes such as
`-rc.1` are also accepted. The workflow runs the locked workspace tests,
builds the native Linux release binary, and publishes these assets:

- `qpwgraph-rs-<version>-x86_64-unknown-linux-gnu.tar.gz`, containing the
  binary, desktop integration files, documentation, and third-party license;
- `qpwgraph-rs-<version>-x86_64.flatpak`, a standalone Flatpak bundle;
- `qpwgraph-rs-<version>-x86_64.AppImage`, a portable Linux application;
- `SHA256SUMS`, covering all release assets.

To publish a release after updating the workspace version:

```bash
git tag -a v0.1.0 -m "Release v0.1.0"
git push origin v0.1.0
```

The same workflow can be rerun from **Actions → Release** for an existing tag.
See [packaging/README.md](packaging/README.md) for download and installation
instructions.

Audio meters are opt-in. They can be off, on demand (the default), or always;
on-demand meters attach while the Slint graph window is visible. PipeWire
helper streams currently report one aggregate reading per node. The backend
API also accepts optional port-associated readings for backends that can expose
independent port buffers; the UI falls back to the node reading. **Reset audio
config** releases every helper stream.

Press F1 for shortcuts. The graph also supports drag-to-connect, rectangle and
multi-selection, node dragging, curved links, scroll-to-pan, zoom, media
filters, port sorting, thumbnail mode, and default node arrangement.

## Effects

The effects API lives in `pw-graph-effects`. It processes interleaved `f32`
audio buffers through a prepare/process/parameter/reset lifecycle. The first
built-in effect is `builtin.noise-gate`, with threshold, attack, hold, release,
and bypass parameters. The demo backend can insert it between a selected audio
link, display it as an effect node, persist its stable endpoint keys, and remove
it while restoring the original link.

Effect channels are processed independently. An enabled effect with an output
connection but no matching input emits a quiet diagnostic noise signal, making
an incomplete route audible instead of silently producing an undefined buffer.

User modules should target `wasm32-unknown-unknown` and implement the exports
documented by `pw_graph_effects::wasm::ABI_DOCUMENTATION`. The realtime ABI has
no WASI imports: module loading, validation, instantiation, and memory growth
belong on the control thread.

## Audio relay

The desktop app includes the relay panel when the default `relay` feature is
enabled. Open **Relay** from the toolbar to show the right-hand Slint overlay.
It provides discovery and refresh, a manual target field, connect and host
start/stop actions, a QR payload action, and one unified device list.
Available, connecting, and connected rows expose the appropriate
join/cancel/disconnect action; connected rows also show their live level.

Opening the panel starts discovery and keeps polling while the window is open.
Relay settings are saved automatically in the app configuration.

Starting a host or connecting to a peer creates two PipeWire virtual nodes:
`qpwgraph-rs.relay.source` exposes received peer audio as **Relay Microphone**,
and `qpwgraph-rs.relay.sink` sends audio routed into **Relay Speaker** to
receiving peers.

### Discovery and pairing

Hosts announce themselves over mDNS (`_qpw-relay._udp`), and USB tether
subnets are probed directly because mDNS often does not cross a tether.
Discovered hosts are listed with their name and endpoint, and connect with
one click. The manual target field accepts a plain `host:port` or a pasted
`qpw-relay://` QR payload for networks where mDNS is blocked.

While the host runs, **Show QR** exposes the same
`qpw-relay://host:port?pin=123456` payload used by the Android app. Active
links are listed first. If interface state flags are incomplete, the UI falls back to a
non-loopback address on the default or physical interface so the endpoint and
QR remain available; transport binding still uses the strict active-link
selection and the operating system's default route.

### Latency

The transport is tuned for minimal delay rather than maximum completeness;
when the two conflict, fresh audio wins.

- **Frame duration defaults to 10 ms** (5–60 ms remain available under
  advanced settings). This halves the codec-side floor at the cost of a
  100 packets/s rate, which local Wi-Fi and USB tether links carry easily.
- **Opus runs in `RESTRICTED_LOWDELAY`**, which drops the SILK layer and its
  5 ms encoder lookahead, with constrained VBR so a loud transient cannot
  inflate a packet. Losses are handled by the receiver's concealment rather
  than by inband FEC, which that mode does not offer.
- **The PCM queues have a working depth, not just a capacity.** A drop-oldest
  queue bounded only by capacity stays full forever after a single consumer
  stall, so every later sample inherits the whole backlog. Both directions now
  trim to roughly four frames, sized by the PipeWire quantum rather than the
  codec frame.
- **The sender parks on a condvar** instead of polling, so a completed frame
  goes on the wire as soon as the capture callback delivers it.
- **The jitter buffer adapts.** It declares a gap lost after one later frame
  on a clean link, widening its tolerance only when packets actually arrive
  late and decaying back after a long clean run.
- **UDP sockets are marked DSCP EF** (voice access category on Wi-Fi) with a
  deliberately small send buffer, and the audio worker threads request
  `SCHED_FIFO` at a priority well below PipeWire's own. All of this is
  best-effort: without `RLIMIT_RTPRIO` the threads simply run as normal ones.

The relay requires the native PipeWire backend. Builds without relay support
remain usable for graph editing, but the relay panel reports that relay is
unavailable. The Android client and native bridge are documented in
[`android/README.md`](android/README.md):

```bash
cargo run -p pw-graph-app --features relay
cargo run -p pw-graph-app --no-default-features --features pipewire
```

## CLI

```text
-m, --minimized       start minimized
-d, --debug           enable debug logging
-n, --no-alsa-midi    disable the optional ALSA MIDI backend
    --lang <LANG>     set the UI language (`en`, `es`, or `fr`)
    --demo            use the deterministic demo backend
```

## Native backends

The PipeWire backend is implemented in Rust in
`crates/pw-graph-backend/src/pipewire.rs` using the official Rust bindings. It
subscribes to registry globals, rebuilds nodes/ports/links, creates links with
`link-factory`, reads `audio.channel` metadata, and provides optional capture
streams for meters. The project has no local C PipeWire shim.

The ALSA backend keeps a small native Sequencer interface and namespaces its
IDs so PipeWire and ALSA graphs can be displayed together. Native development
headers are required when the corresponding features are enabled.

Build without native backends when those libraries are unavailable:

```bash
cargo build --release -p pw-graph-app --no-default-features
cargo build --release -p pw-graph-app --no-default-features --features pipewire
```

## Patchbay files

Files ending in `.qpwgraph` or `.xml` use the qpwgraph XML shape. Other
extensions use JSON. The configured patchbay path is used for startup
activation. Save/load use native dialogs and recent paths are retained in
configuration. Preferences exposes patchbay snapshot/activation and audio
reset actions. Graph connection changes are also written to the active
patchbay path automatically, including effect-node links and undo/redo changes.
Effect-node links are restored with their saved effect instances even when
full patchbay activation is disabled.

## Checks

```bash
cargo fmt --all -- --check
cargo test --workspace
cargo check --workspace
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

To run the GitHub Actions checks locally with `act` and Docker:

```bash
act push -j checks \
  --artifact-server-addr 127.0.0.1 \
  -P ubuntu-latest=catthehacker/ubuntu:act-latest
```
[packaging/README.md](packaging/README.md) for desktop integration notes.
