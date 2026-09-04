# Platform parity

What qpwgraph-rs can do on each backend, what it cannot, and which gaps are
worth closing. This is a status document, not a promise: entries change as the
backends do.

## At a glance

| Feature | Linux | Windows |
| --- | --- | --- |
| Audio devices | PipeWire | Core Audio |
| Audio sessions | PipeWire nodes | Core Audio sessions |
| Arbitrary patch routing | Yes | No for Core Audio |
| Volume, mute, and metering | Yes | Yes, peak metering where available |
| Effects | Yes | Yes, hosted in the router on carried routes |
| MIDI routing | ALSA Sequencer | WinMM, with fan-out and fan-in |
| Relay | Yes, virtual nodes | Yes, direct eCapture/eRender-loopback emitters and eRender receivers |

The rest of this document breaks each of those rows down and says which of the
differences are backlog items and which are facts about the operating system.

## How to read the categories

| Category | Meaning |
| --- | --- |
| **Equivalent** | Both platforms do the same thing, and the user cannot tell which backend is underneath. |
| **Partial** | Works, but with a reduced range, a coarser update, or a missing sub-feature. |
| **Missing** | Not implemented. Nothing structural prevents it; nobody has built it. |
| **Platform limitation** | The platform genuinely does not offer this. No amount of work in this repo changes it. |
| **Bug** | Implemented, but wrong. Needs fixing rather than building. |

The distinction between **Missing** and **Platform limitation** is the important
one: the first is a backlog item, the second is a fact about the operating
system that the UI has to present honestly instead of pretending around.

## Feature status

### Graph and topology

| Feature | Linux (PipeWire + ALSA) | Windows (Core Audio + WinMM) | Status |
| --- | --- | --- | --- |
| Read graph topology | Yes | Yes, as endpoints, sessions, and MIDI devices | Partial: the graph models differ |
| Node/port naming | Yes | Yes | Equivalent |
| Create a connection | Audio and ALSA MIDI | Between audio endpoints, and WinMM MIDI | Partial: application sessions cannot be rewired |
| Remove a connection | Audio and ALSA MIDI | Routes qpwgraph carries, and WinMM MIDI | Partial: same |
| Select an existing connection | Yes | Yes, for observed audio and mutable MIDI links | Equivalent |
| Drag an edge onto another port | Yes | Between audio endpoints, and WinMM MIDI | Partial: same |
| Playback device monitor port | Yes, PipeWire sink monitors | Yes, WASAPI loopback | Equivalent |
| Patchbay persistence | Mutable links | Routes qpwgraph carries, and mutable WinMM MIDI links | Equivalent |

Windows Core Audio has no arbitrary patchbay of its own. What it reports —
which application session is playing to which endpoint — is an observation,
not a link a user can rewire, and there is no supported API to move one.

So qpwgraph carries the audio itself. A link drawn between two endpoint ports
is a real route in `pw-graph-backend::router`, with WASAPI streams at both
ends: the source endpoint is opened for capture (or for loopback, if it is a
playback device's monitor), the destination is opened for render, and the
router moves, converts, and mixes the PCM between them. Disconnecting stops
real audio. See [audio-router.md](audio-router.md).

That makes `connect` and `disconnect` true, and it is worth being precise
about what they cover:

| From | To | Result |
| --- | --- | --- |
| a recording endpoint | a playback endpoint | a real route |
| a playback endpoint's monitor | another playback endpoint | a real route |
| an application session | anything | refused, with an explanation |

The last row is why `node_supports_routing` exists. A backend-wide capability
is a union across what the backend owns, so asking it alone would light up a
connect gesture on a session pin that could only ever fail. The canvas asks
per node instead, and a session pin simply does not offer the gesture.

`is_link_mutable` stays false for every observed session link and true only
for the routes qpwgraph is carrying, so a relationship Windows merely reports
still cannot reach a reroute command or patchbay persistence. Selection and
inspection are unaffected: an observed link is still clickable, still
selectable, and still drawn.

Because carried routes are mutable, they fall into patchbay snapshots and
activation on the same terms as a PipeWire link, keyed by device name rather
than by an id that will not survive a reboot. A snapshot taken while a
microphone is routed to speakers restores that route on the next run.

What remains out of reach from user mode is a qpwgraph-owned endpoint that
*other* applications can select — the virtual microphone the relay needs, and
the destination an arbitrary application could be pointed at. That needs a
driver.

Windows MIDI is a separate native graph. WinMM `midiConnect` and
`midiDisconnect` provide real mutable input-to-output links, so MIDI pins and
links remain draggable, reroutable, and disconnectable even when they share a
canvas with immutable Core Audio relationships.

Fan-out and fan-in are both ordinary. A MIDI input is normally an exclusive
open, so the handles are shared and counted rather than opened per link: a
second connection out of one input reuses the handle WinMM already has and
asks for another pairing, and a handle closes only when its last connection
goes, so removing one branch does not silence the rest. This backend used to
refuse a fanned-out input outright on the assumption that Windows routes one
input to one output; it now lets the MIDI stack answer and reports its error
if the answer is no.

MIDI device graph IDs use the device-interface identity when WinMM provides
one and fall back to a direction/name/driver identity when it does not. The
numeric WinMM index is used only when opening the current device.

Observed Windows Audio links are excluded from patchbay snapshots. Mutable
WinMM MIDI links are included, and missing devices are simply skipped during
later activation rather than being attached to a device that reused an index.

### Audio state and controls

| Feature | Linux (PipeWire) | Windows (Core Audio) | Status |
| --- | --- | --- | --- |
| Set volume | Yes | Yes (endpoint and session) | Equivalent |
| Set mute | Yes | Yes (endpoint and session) | Equivalent |
| Read volume | Yes, from node Props | Yes, endpoint and session | Equivalent |
| Read mute | Yes, from node Props | Yes, endpoint and session | Equivalent |
| Follow external changes | At each rebuild | Yes, event driven | Partial (Linux) |
| Volume above unity | Yes, to 150% | Yes to 150% on a routed node, unity otherwise | Equivalent where qpwgraph owns the audio |
| Per-node capability reporting | Yes | Yes | Equivalent |

The backend owns audio state. `GraphDriver::node_audio_state` returns a
`NodeAudioState` whose `volume` and `muted` are `Option`, where `None` means
"this backend does not currently know". The UI renders that as an unknown
value — a dimmed fader and an explicit unknown mute mark — and never
substitutes a number or boolean of its own. Readability and writability are
tracked independently, so a control can remain actionable without claiming a
value was read successfully.

Two gaps remain here:

PipeWire volume and mute are read back from each node's `Props` during a graph
rebuild, so a level set in pavucontrol or with a media key reaches the cards.
Windows goes further and follows changes by callback, without waiting for a
rebuild; doing the same on Linux would mean holding a param subscription per
node rather than reading on demand. Linux Props subscriptions remain a
separate roadmap item; the current readback preserves missing fields as
unknown.
Windows volume and mute are event driven: `IAudioSessionEvents` and
`IAudioEndpointVolumeCallback` carry the new values in their payload, so the
cache follows a change made anywhere on the system without polling and without
marking the topology dirty. A fader move no longer forces a full endpoint and
session re-enumeration.

Maximum volume is a *node* capability, not audio state: `NodeCapabilities`
carries `volume_max`, and the fader maps its whole travel into whatever range
the node reports, so the top of a fader is never dead travel that silently
clamps.

A Windows endpoint's own control stops at unity, and while nothing is routed
through it that is what it reports. Where qpwgraph is carrying that device's
audio it can make up the difference itself: the endpoint takes
`min(volume, 1.0)` and the route's software gain takes the rest, which
multiply to exactly what was asked for. So a routed node reports 1.5, the same
as PipeWire, and an unrouted one still reports unity — the boost exists
precisely as long as the route does, and is folded back in after every refresh
so it does not appear to collapse when an unrelated device changes.

### Metering

| Feature | Linux (PipeWire) | Windows (Core Audio) | Status |
| --- | --- | --- | --- |
| Meter a capture source | Yes | Yes | Equivalent |
| Meter a playback sink | Yes, through its monitor | Yes | Equivalent |
| Meter an application stream | Yes | Yes where the session exposes a native peak meter | Partial: Windows is peak-only |
| RMS level | Yes | Yes on a routed node, peak-only otherwise | Equivalent where qpwgraph owns the audio |
| Meter policies (off/on-demand/always) | Yes | Yes | Equivalent |
| Meter-only / control-only nodes | Yes | Yes | Equivalent |

Playback sinks used to be excluded from metering on Linux: eligibility required
an audio *source* port, which a sink does not have, so speakers and other output
devices silently showed nothing even though the meter stream already knew how to
read a sink through its monitor. Fixed; `api::is_measurable_audio_node` now
holds the shared PipeWire eligibility rule and is unit-tested. Windows reports
meter capability from the native endpoint/session interface instead.

On Windows, endpoints and sessions are checked independently for
`IAudioMeterInformation`. A session that does not expose it reports no meter
capability rather than being given a meter it can never fill. The available
Core Audio meter is peak-only, which is why a Windows node Core Audio is the
only source for reports `meter_peak: true`, `meter_rms: false`, and
`audio_meters` leaves its `rms` at zero. The UI requests meters from per-node
meter capability, not from the presence of volume controls, and renders
peak-only and meter-only nodes without inventing an RMS bar. Nodes with no
meter capability start in `Unavailable`, not a permanent `Waiting` state.

Where qpwgraph is routing a device, it measured the very samples it carried,
so it reports a real RMS alongside the peak and `meter_rms` becomes true for
that node. The reading is taken at the source, before the route's gain and
before any effect — a microphone's level is what the microphone produced, not
what something downstream made of it, which is the same rule PipeWire follows
by metering a port with the audio that port carries. Routed readings replace
Core Audio's for that node rather than sitting beside it.

Capturing a process's actual PCM stream is *not* reachable by extending
`IAudioSessionControl`. The supported route for that separate feature is
**process loopback capture**:
`ActivateAudioInterfaceAsync` with
`AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK`, which records what one process
tree renders, on build 20348 and newer. The driver already has the bridge it
needs -- `IAudioSessionControl2::GetProcessId` is read for every session -- so
one capture path would serve per-application relay, but it is not required for
the session peak meter described above. It is *Missing*, not a platform
limitation.

An implementation was attempted and reverted: the activation reproducibly
brought down the process with `STATUS_HEAP_CORRUPTION` on this machine, and a
memory-safety fault is not something to ship behind a feature flag. The likely
suspects are the `VT_BLOB` `PROPVARIANT` carrying
`AUDIOCLIENT_ACTIVATION_PARAMS` and the lifetime of that blob across the
asynchronous activation. Worth another attempt against Microsoft's
ApplicationLoopback sample rather than from the API reference alone.

Metering is intentionally conservative on PipeWire. Measuring a node means
attaching a real capture stream, which the session manager links like any other
client — it can resume suspended devices and make the daemon renegotiate the
graph rate. That is why the default policy is on-demand and why meter streams
are flagged passive, monitor-only, and non-reconnecting.

### Processing and networking

| Feature | Linux (PipeWire) | Windows (Core Audio) | Status |
| --- | --- | --- | --- |
| Effect nodes | Yes | Yes, hosted in the router | Equivalent |
| Effect insertion into a link | Yes | Yes, on routes qpwgraph carries | Equivalent |
| Relay: emit local audio | Yes, selected source to Relay Speaker | Yes, physical input or selected render monitor | Partial |
| Relay: receive peer audio | Yes, Relay Microphone to selected sink | Yes, selected render endpoint | Partial |
| Relay: peer audio as a system capture endpoint | Yes, virtual Relay Microphone | No, without an optional driver | Platform limitation |
| Relay: send one application only | Yes | No | Missing (build 20348+) |
| Relay: choose which endpoint | n/a | Yes, by stable endpoint ID | Partial |
| MIDI | ALSA | WinMM, with routing, fan-out, and fan-in | Equivalent for MIDI 1.0 |

Effect *insertion* depends on rewiring an existing link, which is why it
arrived with routing rather than before it. On Windows an effect is a graph
node with an input port, an output port, and a processor in the router between
them, so wiring one up is an ordinary drag and inserting one into a link is
cut-place-reconnect with the original endpoints remembered for restoration.

The same effect gallery, parameters, bypass, and instance identity apply on
both platforms. An effect inserted into one link does not process a sibling
fan-out out of the same source: the router gives each distinct chain its own
branch. What Windows still cannot do is put an effect on a relationship it
merely observes — an application session's stream is not audio qpwgraph owns.

### Desktop integration

| Feature | Linux | Windows | Status |
| --- | --- | --- | --- |
| Tray icon | Yes, StatusNotifier | Yes, `Shell_NotifyIcon` | Equivalent |
| Show / hide / quit from the tray | Yes | Yes | Equivalent |
| Start minimized | Yes | Yes | Equivalent |
| Native file dialogs | Yes | Yes | Equivalent |

Both trays present the same three intents and own no application state: they
send show, hide, and quit back to the Slint event loop and let the normal
window lifecycle apply them. Neither is coupled to anything the audio path
depends on.

The implementations differ because the platforms do. Windows delivers a tray
icon's clicks as a window message, so that tray runs its own hidden window and
message loop on its own thread and posts intents over a channel; the Slint
window is only ever touched from the event loop. Shutting down removes the
icon and joins the thread, so quitting leaves neither a ghost icon in the
notification area nor a thread behind it.

A session with no notification area — a service account, a CI runner — cannot
add an icon. That reports itself by leaving the tray absent rather than by
failing to start: the tray is decoration, not a precondition.

### Relay

The relay engine — pairing, transport, Opus, discovery, QR — is platform
neutral and builds on both targets. Only the audio endpoints that drive
`RelayHandle::push_capture` and `RelayHandle::pull_playback` differ.

On Linux those endpoints are two virtual PipeWire nodes, so *any* application
can be routed into or out of the relay through the patchbay. Emitter mode links
the selected local source to Relay Speaker; Receiver mode links Relay
Microphone to the selected real sink. Default selectors follow WirePlumber's
actual default source or sink, and only qpwgraph-owned automatic links move
when that default changes.

On Windows, Emitter mode opens either a physical input with WASAPI `eCapture`
or a playback monitor with `eRender` loopback. Receiver mode opens an `eRender`
stream for peer audio. The relay panel exposes independent source and sink
lists and persists stable Core Audio IDs; default selectors follow the current
endpoint and a removed explicit device falls back safely. Changing a selection
restarts the one active WASAPI worker, and never leaves a second worker behind.

Windows cannot present received audio as a **microphone** to other applications
without an optional kernel-mode virtual-audio driver. Direct Receiver mode is
still fully usable because it plays peer audio on the selected render endpoint.
Individual applications cannot be selected as Windows relay sources; the
loopback tap is whole-endpoint.

### Refresh and notification behavior

The composite driver gives each child its own refresh responsibility. PipeWire
and Core Audio are refreshed immediately when their event-driven dirty flag is
set, with a five-second safety reconciliation. ALSA MIDI is reconciled on its
own three-second cadence, and WinMM MIDI on a two-second cadence. A MIDI poll
therefore does not force Core Audio to enumerate every endpoint and session.

Core Audio session callbacks retain the owning endpoint ID and enqueue an
endpoint-local session refresh. `OnSessionCreated`, session state changes, and
session display-name changes rebuild only that endpoint's session subgraph;
device and endpoint notifications still request a full topology refresh. Pure
volume and mute callbacks update the shared audio-state cache and do not mark
the graph topology dirty.

## Roadmap

Ordered by how much each one improves what a user actually sees. Everything
above the line has landed; what is left is blocked on something specific.

1. **Windows per-app output routing.** *Blocked on an ABI that cannot be
   derived by probing.* The edge the graph draws between an application session
   and an endpoint is the one relationship Windows lets a user change --
   Settings calls it "App volume and device preferences". The undocumented
   object behind it was probed on 10.0.19045, and these results are worth
   keeping because they narrow the next attempt considerably:

   | Probe | Result |
   | --- | --- |
   | Activate `Windows.Media.Internal.AudioPolicyConfig` | **S_OK** |
   | `IActivationFactory` / `IInspectable` | **S_OK** |
   | IID `{ab3d4648-e242-459f-b02f-541c70306324}` (Windows 11) | E_NOINTERFACE |
   | IID `{2a59116d-6c4f-45e0-a74f-707e3fef9258}` (Windows 10) | **S_OK** |

   So the interface **is present here**, under the Windows 10 IID. What could
   not be established is its method layout. Probing vtable slots past
   `IInspectable`:

   | Slot | Behaviour |
   | --- | --- |
   | 0, 2, 4, 6, 7, 8, 9 | `STATUS_ACCESS_VIOLATION` |
   | 1, 3 | S_OK, no output (consistent with event-removal methods) |
   | 5, 10 | `E_INVALIDARG` |
   | 11 | `E_NOTIMPL` |

   The two slots that survive return `E_INVALIDARG` for **every** combination
   tried: both `eRender` and `eCapture`, all three roles, with and without a
   live audio session for the calling process, and with the first argument as
   both a bare process id and as an `HSTRING` session instance identifier of
   the form Windows actually uses --
   `{0.0.0.…}.{guid}|\Device\…\firefox.exe%b{…}|8%b6528`. Counting the vtable
   by walking entries that stay inside the implementing module does not
   terminate usefully, so the method count could not be recovered that way
   either.

   That is as far as black-box probing goes. A wrong slot is undefined
   behaviour rather than a failed call -- seven of twelve faulted -- and a
   wrong *write* would land in the user's persisted audio settings rather than
   in a crash. The next attempt needs a reference declaration for the
   `{2a59116d-…}` IID, not another guess, and every call should be gated on
   that exact IID so a build exposing a different one reports unsupported
   instead of calling into the wrong slots.
2. **Relay a single application.** *Blocked on the OS build here.* Metering one
   application no longer needs process loopback (see Metering), but capturing
   its audio still does, and that requires build 20348 or newer; this machine
   is 10.0.19045. An attempt from the API reference alone brought the process
   down with `STATUS_HEAP_CORRUPTION`. Needs a newer machine and Microsoft's
   ApplicationLoopback sample rather than the reference.
3. **Linux param subscriptions.** PipeWire controls are read at each rebuild;
   Windows follows them by callback. Holding a `Props` subscription per node
   would close that gap.
4. **Windows per-application effects and metering RMS.** An effect can sit on
   any route qpwgraph carries, but not on an application session, because that
   audio belongs to the Windows audio engine rather than to the router. The
   same boundary applies to RMS: it is real for routed audio and unavailable
   for a session, whose only level is Core Audio's peak-only meter.

## Testing across platforms

Linux and Windows drivers compile on different machines, so no single run can
exercise both. Anything both backends depend on is expressed as platform-neutral
data and tested in `crates/pw-graph-backend/tests/parity_contract.rs`, which
runs everywhere: meter eligibility, meter policy resolution, audio state
semantics, per-node capability reporting, the SPA gain curve, and whether a
backend asks to be polled.

Behaviour that genuinely needs a live daemon stays in each driver's own tests
and is opt-in through environment variables (`PW_GRAPH_TEST_METERS`,
`PW_GRAPH_TEST_LINKS`, `PW_GRAPH_TEST_RELAY`, `PW_GRAPH_TEST_VOLUME`), so an
offline or containerised build does not fail for want of an audio server.

The repository workflow in `.github/workflows/ci.yml` separates native Linux
and Windows checks: formatting, workspace tests, clippy, feature-matrix
compiles, and locked release builds. Native Linux development packages are
installed in the Linux job. These checks do not require a live PipeWire daemon,
physical MIDI hardware, or a Windows endpoint. The manual acceptance checklists
remain separate live smoke tests and are not represented as passed by unit or
CI results.

When adding a rule both backends rely on, put the rule in `api` and test it
there. A shared rule tested only inside one driver is untested on the other
platform.
