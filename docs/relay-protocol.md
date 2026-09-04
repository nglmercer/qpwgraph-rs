# Relay wire protocol, version 3

The relay carries audio between this machine and a peer (typically a phone)
over the local network. It uses two channels:

- a **control channel** over TCP, carrying length-prefixed JSON frames,
- an **audio channel** over UDP, carrying one encoded codec frame per
  datagram.

Version 3 replaces version 2 wholesale. Versions 1 and 2 are not accepted and
there is no downgrade path, because the changes are part of the security model:
version 1
authenticated only the TCP handshake, and its UDP audio channel had no session
identifier, no MAC, and no encryption at all. Anyone who could reach the audio
port could inject audio into a session and — because the host adopted the
source address of any syntactically valid datagram — redirect the session's
outbound audio to themselves, without ever knowing the PIN.

Version 3 also changes the control-channel resume format. A host-wide PIN and a
session id are not enough to resume an existing session; resume additionally
proves possession of a secret derived from that session's original pairing.

## Network transports and discovery

The normal transport is a TCP control socket plus an authenticated and
encrypted UDP audio socket. USB tether discovery is ordinary IPv4 networking
over the active RNDIS/NCM/USB Ethernet interface: the scanner binds each probe
to the USB address that produced its candidate and sends the current protocol
version before accepting a host identity. A probe that disconnects after the
challenge is an abandoned discovery handshake, not a failed PIN attempt.

Every installation advertises a stable random device ID. The ID is an
identifier, not a secret: it lets a previously paired peer be recognized when
its address changes from Wi-Fi to USB. After an explicit PIN pairing, the
client creates a fresh 32-byte trusted credential and sends it inside the
already authenticated control channel. Embeddings persist that credential in
owner-only storage. A later trusted handshake is accepted only when both
stable IDs and the stored credential match; discovery never grants trust to a
new peer by itself.

The desktop application defaults to control port `48123` so a directly scanned
USB tether can find it. An SDK caller may explicitly request port `0` for an
ephemeral listener, but that listener is not discoverable by the fixed USB
probe and must be shared through mDNS or a manual address.

ADB/USB debugging is separate from USB tether networking, but protocol v3 also
has an explicit `adb` transport. It uses the normal TCP listener for control
and a second authenticated TCP connection for length-framed encrypted audio,
so it does not depend on UDP. Android-client-to-desktop-host uses
`adb reverse`; desktop-client-to-Android-host uses `adb forward`. ADB mode is
explicit and has no mDNS/USB peer discovery.

Normal UDP sockets are bound to the selected local interface. A wildcard
(`0.0.0.0`) is used only by `Auto` when no classified relay-capable interface
is available, or when an embedding explicitly requests a wildcard bind. ADB
is loopback-only and never masquerades as a USB network interface.

## Threat model

The relay is designed to be safe on a network containing untrusted devices:
a shared Wi-Fi network, a conference LAN, a phone tether. An attacker is
assumed to be able to see, modify, drop, and inject packets on both channels,
and to know the host's addresses and ports.

What the protocol guarantees against such an attacker:

- **They cannot join a session.** Pairing is a PAKE; without the PIN they
  cannot complete it, and observing any number of attempts does not let them
  test PIN guesses offline.
- **They cannot read or forge traffic.** Both channels are encrypted and
  authenticated with ChaCha20-Poly1305 under keys derived from the pairing.
- **They cannot replay traffic.** The control channel requires strictly
  sequential nonces; the audio channel enforces a sliding replay window.
- **They cannot redirect a session's audio.** The peer's audio address is
  updated only from a datagram that authenticated under the session key.
- **They cannot take over an established session with only the PIN.** Resume
  requires the session-specific secret from the original pairing, a fresh
  challenge, and a proof bound to the session and resume generation.

What it does not defend against: an attacker who learns the PIN, and denial of
service by flooding (the connection and handshake limits bound the cost, they
do not eliminate it).

## Pairing

The host displays a six-digit PIN. Both ends run a symmetric
[SPAKE2](https://datatracker.ietf.org/doc/html/draft-irtf-cfrg-spake2)
exchange over the Ed25519 group with that PIN as the password and the
domain-separating identity `qpwgraph-rs/relay/v3`.

Six digits is a small space, which is exactly why the PIN is never used as a
raw key or MAC key. With a PAKE, an observer of the exchange learns nothing
that lets them test a candidate PIN; guessing requires a fresh online attempt
against the host, and the host allows five failures per source address before
locking it out for a minute.

```text
C → H  Hello        protocol, device_name, device_kind, roles,
                    device_id, transport, sample_rate, channels,
                    pake (client's SPAKE2 message, hex)
H → C  Challenge    protocol, host_name, device_id,
                    pake (host's SPAKE2 message, hex)
C → H  Pair         confirm (client's key confirmation, hex)
H → C  PairConfirm  confirm (host's key confirmation, hex)
```

Both SPAKE2 messages are public; sending the client's in `Hello` keeps pairing
to two round trips.

Each side derives the shared SPAKE2 output, then runs HKDF-SHA256 over it with
the transcript (`client_message || host_message`, in that fixed order on both
sides) as salt to produce seven independent 32-byte values:

| Info string                      | Purpose                            |
| -------------------------------- | ---------------------------------- |
| `qpw-relay control client->host` | Control channel, client to host    |
| `qpw-relay control host->client` | Control channel, host to client    |
| `qpw-relay audio client->host`   | Audio channel, client to host      |
| `qpw-relay audio host->client`   | Audio channel, host to client      |
| `qpw-relay confirm client`       | Client's key confirmation value    |
| `qpw-relay confirm host`         | Host's key confirmation value      |
| `qpw-relay resume authentication v1` | Session-bound resume proof key |

The confirmation values are what turn a completed SPAKE2 run into an
*authenticated* one. A wrong PIN does not make SPAKE2 fail; it makes the two
sides derive different keys. Each end sends the confirmation value the other
end can independently compute and compares the received one in constant time.
A mismatch is a wrong PIN, and the host counts it against the source's
attempt budget.

Every frame on the original post-pairing control channel after `PairConfirm`
is sealed. A reconnecting peer first uses the explicit cleartext resume
challenge flow below; after the proof succeeds, the new control channel is
sealed with freshly derived keys.

### Trusted reconnect and enrollment

An explicit PIN pairing can enroll a long-lived credential. The credential is a
random 32-byte value generated by the client; it is not the PIN and is never
sent in cleartext. The authenticated client sends an enrollment message after
`SessionReady`:

```text
C → H  TrustEnroll       peer_id, secret (hex, sealed)
H → application  TrustedPeerEnrollmentRequested (transaction_id, metadata)
application → H  accept_trusted_enrollment(transaction_id)
H → C  TrustAccepted     (sealed, only after durable commit)
```

`TrustEnroll`, `TrustAccepted`, and `TrustRejected` are valid only inside the
authenticated sealed control channel. The host keeps a bounded 64-entry,
10-second enrollment transaction table. The embedding retrieves the secret
through its private enrollment API, durably commits it, and only then accepts
the transaction. Persistence failure, timeout, malformed credentials, and a
second enrollment for the same peer leave the existing credential unchanged.
A later PIN pairing for the same identity is a transactional rotation: the old
credential remains valid until the new durable commit, then the host replaces
it.

A host configured for PIN-only operation answers with sealed
`TrustRejected` and does not retain the credential.

Desktop persistence is an owner-only atomic config write (temporary sibling,
flush, rename, and parent-directory sync). Android encrypts the credential with
a non-exportable Android Keystore AES-256-GCM key and stores only the version,
nonce, ciphertext, peer ID, and display metadata in `relay.xml`. `relay.xml` is
excluded from backup and device transfer. PINs are never persisted.

For subsequent connections the client proves the credential without a PIN:

```text
C → H  TrustedHello      protocol, device_id, host_id, transport,
                         roles, geometry, client_nonce
H → C  TrustedChallenge  server_nonce, session_id, host_id, host_name
C → H  TrustedProof      proof (HMAC-SHA256, cleartext handshake frame)
H → C  TrustedOk         (cleartext handshake frame)
```

The proof and fresh transport keys bind the client ID, host ID, session ID,
and both nonces. Trusted credentials should be persisted only by the embedding
application in owner-only storage. They are bearer credentials: deleting or
revoking the stored value disables automatic reconnect.

## Control channel

```text
magic "QPR3" (4 bytes) | version u8 = 3 | payload length u32 LE | payload
```

Before key confirmation the payload is JSON. After it, the payload is the
ChaCha20-Poly1305 sealing of the JSON, with the 9-byte header authenticated as
associated data.

Nonces are 12 bytes: a four-byte direction prefix (`QPWc` from the client,
`QPWh` from the host) followed by a little-endian 64-bit counter. TCP delivers
in order, so the receiver requires exactly the next counter — a gap is
tampering, not reordering.

Frames larger than 64 KiB are refused; these are small JSON documents.

Message types after pairing: `PairOk` (audio port and session id),
`SessionStart` / `SessionReady` (negotiated codec and geometry),
`DirectionOffer` / `DirectionAck` (authenticated hot-switch negotiation),
`TrustEnroll` / `TrustAccepted` / `TrustRejected`, `Keepalive` (every 2 s, with a 6 s
timeout), `ControlHint` (volume and mute hints), `ResumeHello` /
`ResumeChallenge` / `ResumeProof` / `ResumeOk`, and `Bye`. Trusted handshakes
use the cleartext `TrustedHello` / `TrustedChallenge` / `TrustedProof` /
`TrustedOk` sequence before starting the sealed session channel.
An unrecognised `type` decodes as `Unknown` rather than killing the
connection, so a newer peer can add messages.

### Direction-first sessions

The public configuration names one audio direction, not a client role:

| Direction | Client-side wire role | Host-side wire role | Audio path |
| --- | --- | --- | --- |
| `mobile_to_desktop` | `emit` only | `receive` only | phone → desktop |
| `desktop_to_mobile` | `receive` only | `emit` only | desktop → phone |

The `roles` fields in `Hello`, `TrustedHello`, and `SessionStart` are the
derived wire representation of that direction. An active session MUST contain
exactly one of `emit` and `receive`; `both` and an empty role set are rejected
before audio workers start. Discovery advertisements may describe capability,
but they do not authorize a bidirectional audio session.

After `SessionReady`, either authenticated peer may propose a direction change.
The messages are sealed control frames and use these JSON shapes:

```json
{"type":"direction_offer","generation":42,"direction":"mobile_to_desktop","device_id":"phone-installation"}
{"type":"direction_ack","generation":42,"direction":"mobile_to_desktop"}
```

An offer is valid only when its stable `device_id` is non-empty and its
direction is one of the two canonical values. Peers resolve offers by comparing
the generation first; at the same generation, the lexicographically greater
stable device ID wins. A same-ID tie is resolved deterministically by the
direction enum order. The loser adopts the winning direction and acknowledges
the resolved generation/direction. Stale generations and a second direction
for an already accepted generation are ignored or rejected; they cannot reverse
a newer authenticated choice.

Direction changes are two-phase. The initiator persists the desired direction
and monotonically increasing generation, sends `DirectionOffer`, and keeps its
current audio endpoint alive until the matching `DirectionAck` or a resolved
winning offer arrives. Both embeddings then stop the old worker, rebuild the
endpoint with the resolved one-way roles, and bring up the new host/client
side. The UI remains in `Switching` until that handoff completes. A timeout
tears down the old endpoint and retries the persisted direction safely rather
than running two audio engines at once.

The pending offer remains in the authenticated session record until its exact
acknowledgement is received. If the control link is resumed or replaced, the
offer is sent again on the new sealed channel before normal keepalive traffic;
this makes a switch requested immediately before a network/process interruption
survive reconnect. If the old session is gone entirely, the persisted direction
and generation are included in the next trusted connection and the same
deterministic resolution rules select one host and one emitter.

### Negotiated parameters

`SessionStart` proposes a codec (`pcm` or `opus`), a frame duration, a sample
rate, and a channel count. The host accepts only:

- frame duration: 5, 10, 20, 40, or 60 ms,
- sample rate: 16 000, 24 000, or 48 000 Hz,
- channels: 1 or 2.

These are the session's geometry, not the machine's. Each end converts between
the session geometry and its own local audio geometry, and mixes the sessions
only once they share a format — so two peers at different rates do not
interfere with each other, and a 16 kHz peer does not play back at three times
the pitch.

## Audio channel

For the normal transport, audio uses UDP as described below. In `adb`
transport, the same sealed datagrams are carried as length-prefixed records on
the authenticated secondary TCP connection:

```text
u32 length (little endian) | sealed audio datagram
```

The secondary connection starts with `AudioHello` (session ID and fresh
client nonce), `AudioChallenge`, `AudioProof`, and `AudioReady`. Its proof is
bound to the session's resume secret, session ID, both nonces, and direction.
The TCP audio stream is replaced when a session resumes; the negotiated audio
keys and playback queues remain the same.

```text
offset 0   u16  magic 0xA1E5
offset 2   u8   version (low nibble) = 2 | flags (high nibble)
                flag 0x10 = stereo, flag 0x20 = keyframe
offset 3   u8   codec id (0 = f32 LE PCM, 1 = Opus)
offset 4   u32  sequence number, one per frame
offset 8   u32  sender timestamp in milliseconds
offset 12  u64  AEAD nonce counter, strictly increasing per sender
offset 20  ..   ChaCha20-Poly1305 ciphertext, then its 16-byte tag
```

The 20-byte header is cleartext — the receiver needs the nonce counter before
it can decrypt — but is authenticated as associated data, so a single flipped
header bit makes the datagram fail to open.

A datagram that does not open is dropped immediately, before anything else
observes it: before the peer-address bookkeeping, before the jitter buffer,
before the decoder. That ordering is the fix for version 1's address-hijacking
flaw.

Nonce counters are checked against a 64-packet sliding window, so genuine
reordering is accepted and replays are not.

An **announce** packet has an empty plaintext (on the wire, a header and a bare
tag). A client sends one right after pairing, and again after a resume, so the
host learns its UDP source address. Because it is sealed with the session key,
only the paired client can move that address — which is what lets a session
survive the phone roaming from Wi-Fi to a USB tether.

A PCM payload must be exactly `frame_samples × 4` bytes. Short, long, and
ragged payloads are dropped rather than partially decoded: in a framed
realtime protocol a wrong-sized packet is a corrupt packet, and accepting one
only perturbs the stream's timing.

## Resume

If the control link drops, the host marks the session resume-eligible for a
15-second grace period. The client re-dials and sends a cleartext
`ResumeHello` containing the session id and a fresh 32-byte client nonce. The
host answers with a fresh 32-byte server nonce and the next resume generation:

```text
C → H  ResumeHello      session_id, client_nonce (hex)
H → C  ResumeChallenge  server_nonce (hex), generation
C → H  ResumeProof      proof (hex)
H → C  ResumeOk         encrypted under fresh control keys
```

The proof is:

```text
HMAC-SHA256(
    resume_authentication_key,
    "qpw-relay resume proof v1"
    || protocol_version
    || session_id
    || generation
    || client_nonce
    || server_nonce
)
```

The `resume_authentication_key` is derived from the original PAKE transcript,
stored only in the session record, and never sent on the wire. The host refuses
resume while the old control watch is still active and allows only one
challenge in flight. A challenge is consumed by the state transition; a proof
from an earlier nonce, generation, or session is invalid, and a successful
resume returns the session to `Active` with a new control generation.

After proof verification both ends derive fresh directional control keys from
the resume secret, session id, generation, and both nonces. Control nonce
counters therefore restart only with genuinely new keys. The old control keys
are not reused, and a host-wide PIN alone cannot resume the session.

Audio keys are *not* rederived: the UDP workers never stopped, and their
nonce counters and replay windows carry on unbroken. In `adb` mode the same
audio workers keep their packet state while a freshly authenticated secondary
TCP stream replaces the old one. The host holds a dropped session open for 15
seconds; the client makes three attempts with exponential backoff. Each
attempt starts with the original target and then tries addresses discovered
for the same stable peer ID, so a Wi-Fi-to-USB address change can be resumed
without treating a different nearby host as the session owner. Candidate
addresses are grouped by stable ID, capped, ranked as last-successful address,
USB, same-subnet, Wi-Fi, Bluetooth PAN, then LAN, and backed off per
`(peer_id, address)`. A public discovery ID is only a routing hint; the resume
proof remains the identity check. A healthy Wi-Fi session is not proactively
moved merely because USB appears; authenticated resume/failover performs the
interface-scoped UDP switch and keeps the old path available if rebinding
fails.

## Resource limits

Every limit below exists because the thing it bounds is reachable by an
unauthenticated peer.

| Limit                       | Default | What it bounds                            |
| --------------------------- | ------- | ----------------------------------------- |
| Concurrent handshakes       | 8       | Threads sitting in a pre-auth read timeout |
| Established sessions        | 16      | Per-session threads and buffers            |
| Pairing failures per source | 5       | Online PIN guessing, then a 60 s lockout   |
| Jitter buffer forward window| 64      | Frames a peer may queue ahead of playback  |
| Jitter buffer capacity      | 128     | Frames held regardless of sequence spread  |
| Control frame size          | 64 KiB  | Allocation per control frame               |
| Event queue                 | 256     | Events held for a slow UI consumer         |
| Trusted enrollment requests | 64      | Durable enrollment transactions            |
| Trusted candidate addresses | 16/peer | Discovery candidates for one identity      |
| Candidate failure records   | 1024    | Per-address reconnect backoff state        |

The host honours an explicit bind address first. Otherwise it binds the highest-
ranked active relay-capable local link under the preference order USB, Wi-Fi,
Bluetooth PAN, then LAN. While hosting, it watches that choice and rebinds the
listener on the same port when the preferred link changes; the existing
sessions, PIN, and stable identity remain in memory. A link-specific listener
also keeps a loopback listener for explicit ADB forwarding. Only when no usable
local-link information is available does the host use the documented all-IPv4
fallback (`0.0.0.0`); an explicit wildcard bind has the same all-interface
semantics.
