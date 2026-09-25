# Video Relay + Android: Future Work (TODO / FUTURE IMPLEMENTATION)

> **Status: TODO / FUTURE IMPLEMENTATION.** Nothing in this document is
> implemented. Network video transport, Android video, and relay protocol v4
> do not exist yet. Relay protocol v3 audio framing is untouched by the video
> work, and `android/` plus `crates/pw-graph-relay-android/` contain no video
> code.

## Intended Linux-to-Android direction

```text
Linux screen
   |
Video filters (pw-graph-video)
   |
H.264 encoder
   |
Relay protocol v4 (video framing, separate from v3 audio)
   |
Android MediaCodec (decode + display)
```

## Intended Android-to-Linux direction

```text
Android MediaProjection
   |
MediaCodec H.264 (encode)
   |
Relay protocol v4
   |
Linux decoder
   |
PipeWire Video/Source (a normal graph node)
```

## Integration seams (already in place)

- `pw_graph_video::VideoFrame`: the decoded-frame handoff an encoder would
  consume. Bounded, validated, format-tagged.
- `pw_graph_video::EncodedVideoFrame` / `VideoCodec`: placeholder types for
  encoded output so relay integration does not reshape the graph.
- `pw_graph_video::recorder::VideoRecorder`: the `VideoFrame -> sink` trait a
  network sender could implement behind a worker thread.
- `PortType::Video` links and patchbay `pipewire-video` persistence: already
  carry video topology; no graph changes needed.

## Explicit non-goals for this phase (do not start)

- MediaProjection capture, MediaCodec encode/decode, JNI video transport.
- Android UI for video.
- Relay protocol v4 design or implementation.
- Any change to relay protocol v3 audio framing.
- H.264 encoder/decoder selection or bundling.

## When this work starts

1. Design relay v4 video framing independently of v3 audio.
2. Pick encoder/decoder crates (or platform APIs) with license review.
3. Implement behind the seams above; keep realtime callbacks free of
   encode/network work (bounded queues, drop-oldest, same as filters).
4. Add opt-in live tests; keep default CI headless.
