# The user-mode audio router

`pw-graph-backend::router` is a deterministic PCM routing engine: it pulls from
sources, applies gain and effects, meters, converts channels and sample rates,
mixes, and pushes to sinks. It knows nothing about PipeWire, Core Audio, nodes,
or ports.

It exists because of one gap. On Linux, qpwgraph asks PipeWire to make a link
and PipeWire moves the audio. Windows has no equivalent: Core Audio will report
which endpoint an application session is attached to and let you change that
endpoint's volume, but it will not let you re-point the session, insert an
effect into a route, or create a capture device.

The way out is for qpwgraph to own the PCM itself. This module is that
ownership, and `windows::routing` is what plugs it into the graph: a link drawn
between two Windows endpoint ports is a route here, with real WASAPI streams at
both ends.

## What it does not do

It is not a driver. The optional Windows driver under
`drivers/windows-audio` is kept in a separate workspace and will only expose
standard endpoint streams; this router remains the single application-level
graph engine.

With WASAPI endpoints on both ends it carries device-to-device audio today —
microphone into speakers, one playback device's monitor into another. What it
cannot do is present a qpwgraph-owned endpoint that *other* applications can
select: the virtual microphone the relay needs, and the destination an
arbitrary application could be pointed at. The driver workspace currently
contains a fail-closed KMDF bootstrap and bounded ring core; the ACX endpoint
circuits still require a real WDK/eWDK build and VM validation.

Capturing a single application is now represented by the Windows
`ProcessLoopbackSource`. It is activated only for a session already assigned
to QPWGraph Virtual Output, so effects and RMS operate on owned PCM without
creating a dry duplicate path.

## The block cycle

One call to `RouterCore::process` moves at most one block:

1. every route pulls a block from its source, once, whatever it feeds;
2. the route's gain is applied;
3. each **branch** takes its own copy, runs its effect chain, and applies its
   own gain;
4. the branch's meter observes the result;
5. for each destination the block is channel-mapped, rate-converted, and
   *added* into that sink's mix accumulator;
6. every sink is written exactly once.

Step 5 is what gives fan-out and mixing from the same code. One source reaching
several sinks is fan-out; several sources reaching one sink is a mix.

Step 3 is what makes "insert an effect into *this* link" mean what it says. A
source feeding a plain destination and an effect-processed one has two
branches, so the effect cannot leak into the sibling path — while the source is
still pulled once, and a chain shared by several destinations still runs once.
`windows::routing` derives the branches by walking the drawn links: paths that
pass through the same effects become one branch, paths that do not become
another.

## Decisions worth knowing

**Meters read post-effect, per branch.** The level is what the destination is
about to receive, not what the source produced. That matches PipeWire, where a
meter sits on the port it is attached to, and it fixes the pre/post ambiguity
cross-platform. A route with several branches has a level per branch, because
the branches genuinely carry different audio; `meter` reports the first and
`branch_meter` the rest.

**RMS is real here.** Windows' `IAudioMeterInformation` is peak-only, which is
why the Core Audio backend reports `rms: 0.0` and declines the RMS capability.
Once the router owns the PCM there is nothing stopping a true RMS, and
`router::meter` computes peak and RMS from the same pass over the block.

**Software gain is not clamped to unity.** A Windows endpoint's own volume
control tops out at unity and is reported as such. Gain above 1.0 lives in the
route qpwgraph owns, which is a separate and honest capability rather than a
claim about the hardware.

**Route tables are replaced atomically.** `set_routes` validates every route
and allocates every buffer before it touches the live table, so a rejected
table leaves the previous one running untouched. A half-applied reroute is a
bug, not a degraded mode.

**Failures are counted, not hidden.** A starved source contributes silence
rather than replaying its last block. A destination that cannot keep up has the
dropped frames counted. A failing effect is bypassed for that block so a bad
parameter does not become unexplained silence. Each of those sets a
`RouteFault` the UI can render.

**Nothing grows without bound.** The device-to-router hand-off is a fixed-size
ring; a consumer that falls behind loses the tail and increments a counter,
rather than trading a dropout for unbounded latency and memory.

## Threading

`RouterCore` is single-threaded, and that is the whole design rather than a
limitation. Devices never call into it: they hand audio over through the
bounded ring in `router::buffer`, and the router pulls on its own thread
(`router::thread`). So a device that stalls cannot stall the router, and
structural changes run between blocks rather than inside a device callback.

`RouterCore::process` itself allocates nothing, locks nothing, and formats no
strings. Control operations — registering a device, replacing the route table,
changing an effect parameter — run as closures on the router thread between
blocks, and anything the core gives up on removal is handed back so it is
dropped on the caller's thread rather than between two blocks of audio.

On Windows, `router::wasapi` opens render, capture, and render-loopback
endpoints. Each gets its own thread owning the COM apartment it initialized and
every interface created in it, the same invariant the Core Audio observation
backend keeps.

If a WASAPI source or sink reports device loss, the paced worker publishes the
route id to the control plane instead of reopening a device from the audio
cycle. The Windows backend drains those notifications during its next graph
refresh, closes the owned worker set only after the route table is empty, opens
the current endpoint identities, reinstalls the link-derived table, and resets
the route's buffers/effects across the discontinuity. A failed reopen remains
degraded and is queued for a later refresh; the link and its diagnostics stay
visible throughout.

## Diagnostics

Every route carries the counters the parity roadmap asks for: source and sink
underruns and overruns, discontinuities, restarts, frames processed, queue
depth, resampler ratio, clock drift in ppm, and per-block timings for the route
and for its effects. They are atomics, so reading them never touches the audio
thread, and the last fault crosses that boundary as a code rather than a
formatted string.

## Clock drift

Two endpoints nominally at 48 kHz are two separate crystals. A converter locked
to the nominal ratio slowly fills or drains the buffer between them until it
breaks, so sinks that can report their own fill level steer the route's
resampler by a few parts per million towards a half-full buffer. The correction
is clamped well below audibility, because an unclamped error term turns a
transient stall into a permanent pitch shift.

## Tests

`crates/pw-graph-backend/src/router/tests.rs` asserts the routing semantics
against in-memory endpoints — no driver, no audio server, no listening. Fan-out,
mixing, gain, channel and rate conversion, effect insertion and bypass,
transactional rollback, device loss, starvation, and metering are all covered,
and they run on every platform in CI.

The Windows side adds tests that need real devices. Most degrade to a skip when
the machine has none, so headless CI stays green; the one that opens WASAPI
streams and moves actual audio is opt-in behind `PW_GRAPH_TEST_LINKS`, like the
equivalent PipeWire test.

`WindowsAudioDriver::route_metrics` reports the same counters per link, which
is how "this link is drawn but carries nothing" becomes a visible fact. A
microphone-into-speakers route on a development machine reads roughly:

```text
frames=37920 underruns=0 overruns=0 fault=None ratio=0.9999846875 process_us=214
```

37,920 frames over 800 ms is 790 ms of audio at 48 kHz, and the ratio shows the
drift controller compensating for about 15 ppm between the two devices' clocks.
Over four seconds the same route reports 191,520 frames and no fault at all.

A route read very soon after connecting can still report `SourceStarved`: the
capture device takes a moment to fill its first buffer, and the route honestly
had a short block. It clears itself once the device is running.
