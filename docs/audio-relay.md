# Audio relay

Carrying audio between this machine and a peer — typically a phone — over the
local network. This page covers the feature as the desktop application exposes
it; the wire format is documented separately in
[Relay wire protocol, version 3](relay-protocol.md).

## The panel

The relay panel supports host start/stop, discovery, peer connection and
disconnection, configurable role/codec/frame/transport, QR payload generation
and parsing, local endpoint discovery, level updates, and virtual relay graph
nodes on Linux.

A host generates a fresh random pairing PIN for each hosting session. It is
shown in the panel and encoded in the QR payload, is never written to disk, and
is retired when the host stops — so a PIN that has been displayed or
photographed does not keep working into the next session.

## USB tethering, Wi-Fi, and ADB

The desktop application's discoverable host default is TCP port `48123`. The
relay SDK still accepts `port(0)` for callers that explicitly want an ephemeral
port, but a USB direct scan cannot discover an ephemeral listener. If
`48123` is occupied, the application reports the bind error; choose another
explicit port and use its address manually.

`Auto` selects the best active local network link in this order: USB/RNDIS/NCM
tether, Wi-Fi, Bluetooth PAN, then LAN. The host listener follows that choice
while it is running: if a preferred interface appears or disappears, the
control listener is rebound on the same port and existing sessions/PIN state
remain intact. It does not silently connect to an untrusted peer. While
discovery is active, mDNS and the bounded direct USB probe run independently;
either one may find a host, and stopping discovery terminates both workers and
clears transient peers.

After a successful explicit PIN pairing, both sides receive a random
per-peer credential. Applications store it in owner-only storage together
with the peer's stable device ID. A later mDNS/USB result for that same ID can
auto-connect without another PIN; an unknown or mismatched peer is never
auto-connected. The desktop panel exposes a global trusted auto-connect switch;
Android enables trusted USB reconnect by default and keeps Wi-Fi reconnect
opt-in. Both clients keep multiple candidates per identity, back off a failed
address independently, and prefer the last successful path followed by USB,
same-subnet, Wi-Fi, Bluetooth PAN, and LAN. Users can forget a trusted device
without restarting; forgetting removes the live credential, stored credential,
and future automatic reconnect permission. The client also associates resume
with that stable ID and can try newly discovered addresses when a host moves
from Wi-Fi to USB.

ADB-only cables are supported through the explicit **ADB** transport. The
client uses the normal TCP listener for control and opens a second authenticated
TCP connection for encrypted, length-framed audio. Android client → desktop
host uses `adb reverse tcp:48123 tcp:48123`; desktop client → Android host uses
`adb forward tcp:48123 tcp:48123`. Select ADB, target `127.0.0.1:48123`, and
create the tunnel first. ADB forwarding is not peer discovery; USB tethering
remains the zero-configuration network workflow. The ADB audio stream has its
own supervisor: loss of the audio TCP connection leaves a healthy control
session alive while the client retries with a fresh authenticated nonce/proof.
If localhost is unreachable, the UI reports that the ADB reverse/forward rule
must be created rather than presenting the failure as a PIN error.

The relay status reports the active transport and link (for example, `udp / usb`,
`udp / wifi`, or `adb-tcp / loopback`) plus the ADB audio-channel state. A
healthy Wi-Fi session is intentionally not proactively migrated just because
USB appears; authenticated resume/failover performs the interface-scoped UDP
switch and keeps the old path available if rebinding fails.

Android platform audio endpoints run stereo PCM16 by default; the mono
geometry stays accepted for callers that explicitly ask for it. Stereo keeps
left/right separation when capturing device playback instead of folding the
mix to one channel. The microphone permission is requested only for Emit,
Both, and Host modes; receive-only playback uses no microphone permission.

## Voice-call audio is not capturable

Device-playback capture cannot pick up call audio — Discord, WhatsApp, Meet,
ordinary phone calls — on any Android version. Those apps play through the
protected voice-communication channel, and Android's playback-capture API is
not allowed to record it: its usage filter accepts only media, game, and
unknown usages, and everything else is excluded by OS privacy policy. This is
a platform rule, not a relay defect, and no app without privileged access can
work around it. To carry a call, emit the phone's microphone to the desktop,
or run the call on the desktop itself.

## Windows

On Windows, the relay uses WASAPI loopback and render streams; the panel
exposes independent capture and playback endpoint choices by stable Core Audio
ID, with system-default fallback when a saved device disappears. Windows cannot
create a microphone endpoint for peer audio, and Windows relay capture is
whole-endpoint rather than per-application.

## Enabling it

The relay is enabled by the default `relay` feature:

```bash
cargo run -p pw-graph-app --features relay
cargo run -p pw-graph-app --no-default-features --features pipewire,relay
```

## Embedding the relay

`pw-graph-relay-sdk` is the stable API for third-party applications, with
`RelayHostBuilder` and `RelayClientBuilder` as the entry points.
`pw-graph-relay-android` wraps that SDK in JNI bindings for Android.

SDK hosts receive `RelayEvent::TrustedPeerEnrollmentRequested`. They must call
`trusted_enrollment_secret`, persist the secret in their own durable private
store, then call `accept_trusted_enrollment`; otherwise call
`reject_trusted_enrollment`. `TrustAccepted` is never sent for an in-memory-only
credential. `remove_trusted_peer` revokes a credential from the live engine and
the embedding should remove its durable record as well.

`.audio(sample_rate, channels, frame_ms)` sets both the negotiated wire
geometry and the local geometry of the PCM you push and pull; use
`.wire_audio()` / `.local_audio()` only when the two genuinely differ.
Geometries outside the negotiable set (16/24/48 kHz, mono or stereo,
5/10/20/40/60 ms) are rejected at `build()`.

## Related

- [Relay wire protocol, version 3](relay-protocol.md) — frames, pairing,
  resume authentication, and crypto.
- [Audit follow-ups](audit-follow-ups.md) — resolved audit items and regression
  coverage.
