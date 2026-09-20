# Task: Implement Windows app + microphone routing to virtual microphone

Repository:

`https://github.com/nglmercer/qpwgraph-rs`

## Goal

Implement the Windows equivalent of the common PipeWire workflow:

```text
Music App + Physical Microphone
            |
            v
        QPWGraph Mixer
            |
            v
 QPWGraph Relay Microphone
            |
            v
         Discord
```

Keep routing, mixing, effects, gain, metering, resampling, and fan-out in user space.

The kernel driver must only expose virtual Windows audio endpoints and transport PCM.

## Architecture

Use the existing components:

```text
ProcessLoopbackSource ─┐
                      ├─> RouterCore ─> Relay Sink ─> Relay Microphone
Physical Mic WASAPI ──┘
```

Do not implement a PipeWire-like kernel mixer.

## 1. Per-application capture

Implement/complete a Windows `ProcessLoopbackSource`.

Use:

* `ActivateAudioInterfaceAsync`
* `AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS`
* WASAPI process loopback capture

Requirements:

* Capture one selected process/application.
* Identify applications by stable selector where possible.
* Do not capture unrelated system audio.
* Session termination must cleanly stop the source.
* Do not silently switch to another process.
* Feed PCM into the existing router abstraction.

Preferred internal format:

```text
48 kHz
f32
stereo when applicable
bounded buffers
```

Reuse the router's existing conversion/resampling pipeline rather than duplicating it.

## 2. Physical microphone source

Expose physical capture endpoints as router sources using WASAPI `eCapture`.

Both sources must be usable simultaneously:

```text
ProcessLoopbackSource
WasapiCaptureSource
```

## 3. Mixing

Use the existing:

`pw-graph-backend::router::RouterCore`

Do not create a separate Windows mixer.

Required behavior:

```text
music ── gain ──┐
                ├─> mix -> destination
mic ─── gain ───┘
```

Support:

* independent gain
* mute
* effects
* resampling
* channel conversion
* fan-out
* peak/RMS metering
* bounded buffering
* underrun/overrun diagnostics

## 4. Virtual microphone path

Use the existing Windows driver under:

`drivers/windows-audio`

Keep the existing ACX/KMDF architecture.

Required endpoint pair:

```text
QPWGraph Relay Sink
        |
        | bounded PCM cable
        v
QPWGraph Relay Microphone
```

Roles:

```text
relay-render  = Relay Sink
relay-capture = Relay Microphone
```

QPWGraph writes mixed PCM to `Relay Sink`.

Applications such as Discord must see `Relay Microphone` as a normal Windows recording device.

Do not put mixing/effects/routing logic inside the driver.

## 5. Virtual application output

Complete/support the second existing endpoint pair:

```text
QPWGraph Virtual Output
        |
        | bounded PCM cable
        v
QPWGraph Virtual Monitor
```

Roles:

```text
app-render  = Virtual Output
app-monitor = Virtual Monitor
```

This enables:

```text
Spotify -> Virtual Output -> QPWGraph Router
```

Then QPWGraph may route the stream to:

```text
Headphones
Relay Microphone
Recorder
Effects
multiple destinations
```

## 6. Routing behavior

Support graphs equivalent to:

```text
Spotify ─────┬─> Headphones
             ├─> Relay Microphone
             └─> Recorder

Microphone ──┬─> Relay Microphone
             └─> Recorder
```

Each source must be captured once and fan out through `RouterCore`.

Do not duplicate capture workers per destination.

## 7. Application routing policy

Do not depend on undocumented Windows audio-policy APIs for core functionality.

`IAudioPolicyConfig` / private Windows audio policy APIs may remain optional/experimental.

Supported baseline workflow:

```text
Windows Volume Mixer
Spotify output -> QPWGraph Virtual Output
```

Process loopback must still work without moving the application's output.

## 8. Driver constraints

Keep the driver minimal.

Driver responsibilities:

```text
Expose ACX endpoints
Advertise supported formats
Maintain bounded PCM cables
Handle stream lifecycle
Handle timing/packet movement
Expose stable endpoint roles
```

Driver must NOT:

```text
mix streams
apply effects
route graph edges
manage application sessions
perform UI logic
```

Preserve:

* existing ACX implementation
* WDK/eWDK build separation
* test-signing workflow
* Secure Boot/release gates
* Driver Verifier tests
* HLK gates
* bounded transport
* fail-closed behavior when driver validation is incomplete

## 9. Threading

Audio callbacks/workers must:

* avoid unbounded allocations
* avoid UI access
* avoid blocking locks where possible
* use bounded queues/rings
* never allow latency to grow without bound

COM interfaces must remain owned by the thread/apartment that created them.

Structural route changes must happen outside real-time callbacks.

## 10. UI

Expose Windows sources including:

```text
Microphones
Playback monitor endpoints
Live applications
QPWGraph Virtual Monitor
```

Allow users to connect:

```text
Application -> Relay Microphone
Microphone   -> Relay Microphone
```

When multiple sources feed the same destination, show it as a normal mixed route.

Expose per-link/source:

```text
gain
mute
effects
peak
RMS where PCM is available
fault state
```

## 11. Discord use case

Acceptance workflow:

```text
1. Start Spotify.
2. Start QPWGraph.
3. Select Spotify as process-loopback source.
4. Connect Spotify -> Relay Microphone.
5. Connect physical microphone -> Relay Microphone.
6. Select "QPWGraph Relay Microphone" in Discord.
7. Discord receives both microphone and Spotify.
8. Local Spotify playback continues normally.
```

No application rerouting should be required for this workflow.

## 12. Full virtual-output workflow

Also support:

```text
1. Set Spotify output to "QPWGraph Virtual Output".
2. QPWGraph receives PCM through Virtual Monitor.
3. Route it to headphones and Relay Microphone.
4. Spotify must not produce a duplicate dry path.
5. Effects may be inserted before either destination.
```

## 13. Failure handling

Handle gracefully:

* application exits
* endpoint disappears
* default device changes
* driver endpoint disappears
* device invalidation
* capture starvation
* render overrun
* sample-rate mismatch
* channel-layout mismatch
* router restart

Never silently route a stale application selector to another process.

Preserve route diagnostics and existing restart/recovery behavior.

## 14. Tests

Add tests for:

```text
process loopback -> router
mic + process mixing
fan-out
gain/mute
sample-rate conversion
endpoint loss
process termination
virtual relay round trip
virtual output round trip
bounded-buffer overflow/underflow
```

Use existing opt-in live Windows tests where physical devices or the driver are required.

Keep normal CI usable without installed virtual hardware.

## 15. Implementation priorities

Implement in this order:

```text
1. ProcessLoopbackSource
2. ProcessLoopbackSource -> RouterCore
3. Physical Mic + ProcessLoopback mixing
4. RouterCore -> Relay Sink
5. Relay Sink -> Relay Microphone validation
6. Discord-compatible end-to-end test
7. Virtual Output -> Virtual Monitor integration
8. Full graph routing/fan-out
9. UI improvements
10. Optional automatic app reassignment
```

## Constraints

Do not:

* port PipeWire to Windows
* implement routing inside the kernel driver
* introduce another Windows-specific mixer
* rely on undocumented APIs for required functionality
* use unbounded audio queues
* duplicate PCM processing already implemented in `RouterCore`
* silently fall back to another process/session
* break Linux behavior
* break existing relay functionality
* break current Windows driver packaging/validation

## Definition of done

The implementation is complete when:

```text
Spotify/process audio + physical microphone
                |
                v
           RouterCore
                |
                v
       Relay Microphone
                |
                v
             Discord
```

works reliably on Windows, while the same router architecture remains shared with the rest of QPWGraph.
