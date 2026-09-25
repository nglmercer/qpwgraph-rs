# Linux Video (PipeWire)

First-class PipeWire video support on Linux: capture, filter, preview, and
(supported compositors) virtual displays. Video lives in the same graph as
audio — ports are typed `PortType::Video` and the existing compatibility
checks forbid `Audio <-> Video` links.

## Pipeline

```text
PipeWire Video Sources (camera, screen-cast stream, filter output)
        |
        v
   Video Filters (worker threads, bounded queues)
        |
        +--> Video Output (PipeWire video sink nodes)
        +--> Preview (newest-frame dialog)
        +--> Recorder (abstraction only, see below)
        +--> Future Relay Video (see video-relay-future.md)
```

```text
Linux Monitor / Window
        |
        v
XDG ScreenCast Portal (permission dialog, never bypassed)
        |
        v
PipeWire Video Source
```

```text
Virtual Monitor (where the compositor offers SourceType::VIRTUAL)
      |
      v
XDG ScreenCast Portal VIRTUAL
      |
      v
PipeWire Video
```

## Crates and modules

- `crates/pw-graph-video`: realtime-safe video core. `VideoSpec` /
  `VideoPixelFormat` (validated, bounded), `VideoFrame` (exactly-sized),
  `EncodedVideoFrame` (relay placeholder), `VideoQueue` (bounded,
  drop-oldest), `VideoProcessor` + `VideoWorker` (off-thread processing with
  bypass-on-failure), `filters/` (passthrough, grayscale, hflip, vflip,
  crop, scale — CPU only), `preview` (newest-frame slot), `recorder`
  (abstraction + null implementation), `diagnostics` (lock-free counters).
- `crates/pw-graph-backend/src/video.rs`: `VideoDriver` trait (default
  `Unsupported`, so non-Linux backends are untouched).
- `crates/pw-graph-backend/src/linux/screencast.rs`: portal session state
  machine (`Idle/Requesting/Active/CancelledByUser/SessionClosed/SourceGone/
  Failed`), stream identity tracking (serial preferred over transient node
  id), and the live `ashpd` connector behind the `screencast` cargo feature.
- `crates/pw-graph-backend/src/pipewire/video/`: SPA `EnumFormat`
  negotiation (`format.rs`), capture streams (`capture.rs`), output streams
  (`output.rs`), and filter bridges (`bridge.rs`).
- UI: `crates/pw-graph-slint/src/bridge/video.rs`, the `VideoPreviewDialog`,
  the effects dialog video tab, per-card video actions (preview on filter
  and capture cards, stop on the capture card), and per-card video
  subtitles (resolution, fps, format, state, dropped frames). The rail
  keeps only capture monitor/window and virtual display creation —
  everything with a node lives on the node or in the dialog.
- Webcams (V4L2 and libcamera) are detected by `media.role=Camera` with
  `v4l2_input.`/`libcamera_input.` name-prefix fallback, render with the
  camera icon fallback and the localized camera name, and route through
  the normal video ports. Live webcam preview is not implemented yet.

## Video filter nodes

One filter is one synthetic graph node (`video_in` + `video_out`) backed by
a capture stream retargeted at the upstream node, a worker thread, and an
output stream. Links project onto PipeWire as:

- `real video out -> bridge in`: capture retarget, no daemon link.
- `bridge out -> real video in`: daemon link from the output stream.
- `bridge A out -> bridge B in`: B captures A's output stream node.

The output stream is created lazily once the input spec negotiates, and
recreated on renegotiation. A failing processor bypasses (forwards input)
instead of crashing; stream stalls and format mismatches surface as
`bypassed`/`failed` node states with diagnostics intact.

## Screen capture

Default builds include live portal access via `ashpd` (Linux):

```text
CreateSession -> SelectSources -> Start -> OpenPipeWireRemote
```

The compositor permission dialog is always shown; cancellation is a clean
terminal state. One OS thread per session drives the ashpd session and the
PipeWire remote FD with `pollster::block_on`, so no D-Bus lifetime crosses
threads. Handled cleanly: user cancel, session close/revocation, window
close, monitor unplug (stream disappearance), compositor restart, and
PipeWire stream renegotiation.

Without the `screencast` feature (backend-only or `--no-default-features`
builds), capture reports unavailability and the same state machine stays
unit-tested with scripted connectors.

## Virtual displays

`Create Virtual Display` probes `AvailableSourceTypes` for
`SourceType::VIRTUAL` at attempt time and caches the answer. Support is
never assumed: unsupported compositors get a clear status message, not a
silent fallback. No privileged system modifications are ever attempted.

Resolution/refresh input: `WxH@Hz` (default `1920x1080@60`); invalid values
fall back to the default rather than failing.

## VKMS (optional future/advanced fallback)

VKMS (Virtual Kernel Mode Setting) can provide a virtual DRM connector that
a compositor may expose as a monitor, which can then be captured through the
ordinary ScreenCast flow. It is explicitly **not** part of this
implementation:

- Portal virtual displays are the primary and only implemented path.
- VKMS is not required, not configured, and no kernel module is ever loaded
  automatically.
- A future advanced fallback could document manual `modprobe vkms` +
  compositor-specific enablement steps for headless/test rigs, with capture
  still going through the portal. That work is not started.

Do not build a custom kernel module or display driver for this project.

## Recording status

Local encoded recording is **not implemented** in this phase. The stable
seam is `pw_graph_video::recorder::VideoRecorder` (`prepare`/`record`/
`finish` + `NullVideoRecorder`), designed to be fed from a worker thread —
never from a realtime callback. A future encoder (container + codec)
implements that trait without changing capture, processing, or the graph.

## Diagnostics

Per node: frames received/processed/output/dropped/bypassed, queue
depth/capacity, last/max processing time, current width/height/fps/format,
and last error. No per-frame logging in normal operation.

## Limits

- 7680x4320, 240 fps, 128 MiB per frame, queue depth 1..=8 (default 3).
- Dimensions from PipeWire are validated before any allocation; planar
  formats require even dimensions.
