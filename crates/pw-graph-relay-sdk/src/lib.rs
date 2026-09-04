//! `pw-graph-relay-sdk` — the stable public surface of the qpwgraph-rs
//! audio relay.
//!
//! This crate is what third-party applications depend on. It wraps the
//! [`pw_graph_relay`] engine with direction-oriented builders:
//!
//! - [`RelayHost`] — run on a PC: accepts phone/peer connections for one
//!   selected direction and exposes the matching audio queue.
//! - [`RelayClient`] — run anywhere (Linux desktop, Android via JNI): carry
//!   exactly one selected direction, either captured audio to a host or host
//!   audio to playback.
//!
//! The SDK is audio-IO agnostic: you push captured PCM and pull playback PCM.
//! That keeps it portable — PipeWire on Linux, AAudio/OpenSL ES on Android,
//! WASAPI/CoreAudio later.
//!
//! ## Host example
//!
//! ```no_run
//! use pw_graph_relay_sdk::{RelayDirection, RelayHostBuilder};
//!
//! let host = RelayHostBuilder::new()
//!     .device_name("studio-pc")
//!     .pin("123456")
//!     .direction(RelayDirection::MobileToDesktop)
//!     .build()
//!     .expect("builder")
//!     .start()
//!     .expect("host starts");
//! println!("listening on port {}", host.port());
//! // In the Mobile → Desktop audio loop:
//! //   let mut buffer = [0.0f32; 960];
//! //   let n = host.pull_playback(&mut buffer); // peer mic audio
//! ```
//!
//! ## Client example (phone-as-microphone)
//!
//! ```no_run
//! use pw_graph_relay_sdk::{RelayClientBuilder, RelayDirection};
//!
//! let client = RelayClientBuilder::new()
//!     .direction(RelayDirection::MobileToDesktop)
//!     .build()
//!     .expect("builder")
//!     .connect("192.168.1.20:48123", "123456")
//!     .expect("connect");
//! // In your capture loop: client.send_capture(&mic_pcm);
//! // In your render loop: client.pull_playback(&mut buffer);
//! ```
//!
//! Wire protocol: see `docs/relay-protocol.md` in the qpwgraph-rs
//! repository.

pub use pw_graph_relay::{
    generate_device_id,
    netlink::{display_links, listen_bind_addr, local_links, select_links},
    CodecKind, DeviceKind, EngineConfig, EngineStatus, FlowAck, FlowOffer, LinkKind, LocalLink,
    LocalRelayMode, PeerInfo, RelayDirection, RelayError, RelayEvent, RelayFlow, RelayMode,
    RelayResult, Roles, SessionId, SessionStatus, TransportPreference, TrustedPeer,
    FRAME_DURATIONS_MS, MAX_DISCOVERED_PEER_ADDRESSES, MAX_REALTIME_QUANTUM_SAMPLES,
    MAX_TRUSTED_PEERS, SAMPLE_RATES_HZ,
};
// `RelayHost::handle`/`RelayClient::handle` return this, so it has to be
// nameable by callers holding one.
pub use pw_graph_relay::RelayHandle;

use pw_graph_relay::RelayEngine;
use std::net::{SocketAddr, ToSocketAddrs};
use std::time::{Duration, Instant};

/// Reject an audio geometry the session negotiation would refuse anyway.
///
/// Builders used to accept any numbers at all and only fail once a handshake
/// reached `validate_negotiation`, which on the client side is after a TCP
/// connect and a PAKE — a long way to travel to learn that `44_100` is not a
/// negotiable rate. Validating at `build()` keeps the failure next to the
/// mistake. Nothing is silently normalised: an unsupported value is an error,
/// not something quietly rounded into a different format than the caller
/// asked for.
fn validate_audio(sample_rate: u32, channels: u16, frame_ms: u16) -> RelayResult<()> {
    if !pw_graph_relay::is_supported_sample_rate(sample_rate) {
        return Err(RelayError::Config(format!(
            "unsupported sample rate {sample_rate} Hz; supported: {SAMPLE_RATES_HZ:?}"
        )));
    }
    if !pw_graph_relay::is_supported_channels(channels) {
        return Err(RelayError::Config(format!(
            "unsupported channel count {channels}; supported: 1 (mono) or 2 (stereo)"
        )));
    }
    if !pw_graph_relay::is_supported_frame_ms(frame_ms) {
        return Err(RelayError::Config(format!(
            "unsupported frame duration {frame_ms} ms; supported: {FRAME_DURATIONS_MS:?}"
        )));
    }
    Ok(())
}

fn legacy_direction_for_client_mode(mode: RelayMode) -> RelayDirection {
    match mode {
        RelayMode::Emitter => RelayDirection::MobileToDesktop,
        RelayMode::Receiver => RelayDirection::DesktopToMobile,
    }
}

fn legacy_direction_for_host_mode(mode: RelayMode) -> RelayDirection {
    match mode {
        RelayMode::Emitter => RelayDirection::DesktopToMobile,
        RelayMode::Receiver => RelayDirection::MobileToDesktop,
    }
}

/// Builder for [`RelayHost`].
#[derive(Clone, Debug)]
pub struct RelayHostBuilder {
    config: EngineConfig,
}

impl RelayHostBuilder {
    #[allow(deprecated)]
    pub fn new() -> Self {
        Self {
            config: EngineConfig {
                mode: RelayMode::Receiver,
                direction: legacy_direction_for_host_mode(RelayMode::Receiver),
                client_roles: Roles::receive_only(),
                ..Default::default()
            },
        }
    }

    pub fn device_name(mut self, name: impl Into<String>) -> Self {
        self.config.device_name = name.into();
        self
    }

    /// Stable installation identity used by discovery and trusted
    /// reconnects. Leave the generated default unless an embedding
    /// application already has durable device identity storage.
    pub fn device_id(mut self, id: impl Into<String>) -> Self {
        self.config.device_id = id.into();
        self
    }

    pub fn trusted_peers(mut self, peers: impl IntoIterator<Item = TrustedPeer>) -> Self {
        self.config.trusted_peers = peers.into_iter().collect();
        self
    }

    pub fn trust_new_peers(mut self, enabled: bool) -> Self {
        self.config.trust_new_peers = enabled;
        self
    }

    /// User-facing direction served by this host. Session roles are derived
    /// internally from the direction; callers should configure connecting
    /// peers with the same direction.
    #[allow(deprecated)]
    pub fn direction(mut self, direction: RelayDirection) -> Self {
        self.config.direction = direction;
        self.config.client_roles = Roles::for_direction(direction);
        self.config.mode = match direction {
            RelayDirection::MobileToDesktop => RelayMode::Receiver,
            RelayDirection::DesktopToMobile => RelayMode::Emitter,
        };
        self
    }

    /// Select the host's local generic role. The host exposes one-way audio
    /// through `receive_audio` or `send_audio` accordingly.
    #[allow(deprecated)]
    pub fn mode(mut self, mode: RelayMode) -> Self {
        self.config.direction = legacy_direction_for_host_mode(mode);
        self.config.client_roles = mode.roles();
        self.config.mode = mode;
        self
    }

    /// Monotonic persisted direction generation used during trusted resume.
    pub fn direction_generation(mut self, generation: u64) -> Self {
        self.config.direction_generation = generation;
        self.config.mode_generation = generation;
        self
    }

    /// Advertised device kind (default: [`DeviceKind::Linux`]).
    pub fn device_kind(mut self, kind: DeviceKind) -> Self {
        self.config.device_kind = kind;
        self
    }

    /// Pairing PIN clients must present. Required before [`Self::start`]. The
    /// SDK treats this as caller-owned configuration: it is neither persisted
    /// nor silently regenerated across starts.
    pub fn pin(mut self, pin: impl Into<String>) -> Self {
        self.config.pin = pin.into();
        self
    }

    /// TCP control port; 0 picks an ephemeral port. The SDK deliberately
    /// keeps that opt-in behavior; the desktop application's discoverable
    /// default is 48123.
    pub fn port(mut self, port: u16) -> Self {
        self.config.port = port;
        self
    }

    pub fn codec(mut self, codec: CodecKind) -> Self {
        self.config.codec = codec;
        self
    }

    /// Preferred transport link (default: auto-select the best available).
    pub fn transport(mut self, transport: TransportPreference) -> Self {
        self.config.transport = transport;
        self
    }

    /// Audio format exchanged with connected peers *and* the format of the
    /// PCM the embedding application pushes and pulls.
    ///
    /// Embedding applications drive their own audio IO (e.g.
    /// `AudioRecord`/`AudioTrack` on Android) and push/pull PCM at this rate,
    /// so this sets both the negotiated wire geometry and the engine's local
    /// geometry. Setting only the wire half — as this used to — left the
    /// engine interpreting an SDK caller's interleaved stereo buffers as mono
    /// at the default 48 kHz, which is audible corruption rather than a
    /// mis-negotiation. Use [`Self::wire_audio`] afterwards if the two really
    /// do differ.
    ///
    /// Invalid geometries are rejected at [`Self::build`], not here, so the
    /// builder stays chainable.
    pub fn audio(mut self, sample_rate: u32, channels: u16, frame_ms: u16) -> Self {
        self.config.sample_rate = sample_rate;
        self.config.channels = channels;
        self.config.frame_ms = frame_ms;
        self.config.local_sample_rate = sample_rate;
        self.config.local_channels = channels;
        self
    }

    /// Override *only* the negotiated wire geometry, leaving the local
    /// application geometry set by [`Self::audio`] alone.
    ///
    /// Call this after [`Self::audio`]: the engine then converts between the
    /// two, which is the supported way to, say, run 48 kHz stereo endpoints
    /// while sending 16 kHz mono over a constrained link.
    pub fn wire_audio(mut self, sample_rate: u32, channels: u16) -> Self {
        self.config.sample_rate = sample_rate;
        self.config.channels = channels;
        self
    }

    /// Override *only* the local application geometry — the format of the PCM
    /// passed to `push_capture` and filled by `pull_playback`.
    pub fn local_audio(mut self, sample_rate: u32, channels: u16) -> Self {
        self.config.local_sample_rate = sample_rate;
        self.config.local_channels = channels;
        self
    }

    pub fn build(self) -> RelayResult<RelayHostPrepared> {
        validate_audio(
            self.config.sample_rate,
            self.config.channels,
            self.config.frame_ms,
        )?;
        validate_audio(
            self.config.local_sample_rate,
            self.config.local_channels,
            self.config.frame_ms,
        )?;
        Ok(RelayHostPrepared {
            config: self.config,
        })
    }
}

impl Default for RelayHostBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// A validated host that has not started listening yet.
/// A host configuration ready to start. Cloneable so a failed `start` does
/// not consume the caller's only copy: retrying should not require rebuilding
/// the configuration from scratch.
#[derive(Clone)]
pub struct RelayHostPrepared {
    config: EngineConfig,
}

impl RelayHostPrepared {
    /// Start listening. Returns the running host.
    pub fn start(self) -> RelayResult<RelayHost> {
        if self.config.mode != RelayMode::Receiver {
            return Err(RelayError::Config(
                "a relay host must use Receiver mode".into(),
            ));
        }
        let engine = RelayEngine::start(self.config)?;
        let handle = engine.handle();
        let port = handle.host_start()?;
        Ok(RelayHost {
            _engine: engine,
            handle,
            port,
        })
    }
}

/// A running relay host.
pub struct RelayHost {
    _engine: RelayEngine,
    handle: RelayHandle,
    port: u16,
}

impl RelayHost {
    /// The TCP control port peers connect to.
    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn handle(&self) -> RelayHandle {
        self.handle.clone()
    }

    /// Audio received from emitting peers (e.g. phone microphones). This is
    /// the active host queue for `MobileToDesktop`.
    pub fn pull_playback(&self, out: &mut [f32]) -> usize {
        self.handle.pull_playback(out)
    }

    /// Receive peer PCM while this host is configured as a Receiver.
    pub fn receive_audio(&self, out: &mut [f32]) -> usize {
        self.pull_playback(out)
    }

    /// Realtime-safe variant of [`Self::pull_playback`]. It returns zero when
    /// an engine lock is busy or no audio is available, may return a partial
    /// quantum, and never produces more than
    /// [`MAX_REALTIME_QUANTUM_SAMPLES`] samples. An oversized output slice is
    /// short-served and its tail is untouched.
    pub fn try_pull_playback(&self, out: &mut [f32]) -> usize {
        self.handle.try_pull_playback(out)
    }

    /// Audio to broadcast to receiving peers (e.g. the PC's relay sink tap).
    /// This is the active host queue for `DesktopToMobile`.
    pub fn push_capture(&self, samples: &[f32]) {
        self.handle.push_capture(samples);
    }

    /// Provide local PCM while this host is configured as an Emitter.
    pub fn send_audio(&self, samples: &[f32]) {
        self.push_capture(samples);
    }

    /// Realtime-safe variant of [`Self::push_capture`]. It returns `false`
    /// when the input exceeds [`MAX_REALTIME_QUANTUM_SAMPLES`], a realtime
    /// lock is busy, or no accepting session is available; otherwise the
    /// complete input quantum is offered to each bounded session queue.
    pub fn try_push_capture(&self, samples: &[f32]) -> bool {
        self.handle.try_push_capture(samples)
    }

    /// Drain pending events (session established/lost, levels, errors).
    pub fn events(&self) -> Vec<RelayEvent> {
        self.handle.events()
    }

    pub fn status(&self) -> EngineStatus {
        self.handle.status()
    }

    /// End one session.
    pub fn disconnect(&self, session: SessionId) -> RelayResult<()> {
        self.handle.disconnect(session)
    }

    /// Propose a new direction for an authenticated session. The embedding
    /// must wait for `DirectionResolved` before replacing its audio worker.
    pub fn offer_direction(
        &self,
        session: SessionId,
        direction: RelayDirection,
        generation: u64,
    ) -> RelayResult<()> {
        self.handle.offer_direction(session, direction, generation)
    }

    pub fn remove_trusted_peer(&self, peer_id: &str) -> RelayResult<()> {
        self.handle.remove_trusted_peer(peer_id)
    }

    pub fn trusted_peers(&self) -> Vec<TrustedPeer> {
        self.handle.trusted_peers()
    }
}

/// Builder for [`RelayClient`].
#[derive(Clone, Debug)]
pub struct RelayClientBuilder {
    config: EngineConfig,
}

impl RelayClientBuilder {
    pub fn new() -> Self {
        Self {
            config: EngineConfig::default(),
        }
    }

    pub fn device_name(mut self, name: impl Into<String>) -> Self {
        self.config.device_name = name.into();
        self
    }

    pub fn device_id(mut self, id: impl Into<String>) -> Self {
        self.config.device_id = id.into();
        self
    }

    pub fn trusted_peers(mut self, peers: impl IntoIterator<Item = TrustedPeer>) -> Self {
        self.config.trusted_peers = peers.into_iter().collect();
        self
    }

    pub fn trust_new_peers(mut self, enabled: bool) -> Self {
        self.config.trust_new_peers = enabled;
        self
    }

    pub fn device_kind(mut self, kind: DeviceKind) -> Self {
        self.config.device_kind = kind;
        self
    }

    /// Direction of this client's audio endpoint. Mobile → Desktop is
    /// emit-only; Desktop → Mobile is receive-only.
    #[allow(deprecated)]
    pub fn direction(mut self, direction: RelayDirection) -> Self {
        self.config.direction = direction;
        self.config.client_roles = Roles::for_direction(direction);
        self.config.mode = match direction {
            RelayDirection::MobileToDesktop => RelayMode::Emitter,
            RelayDirection::DesktopToMobile => RelayMode::Receiver,
        };
        self
    }

    /// Select this client's local generic role. The resulting handshake is
    /// always `emit_only` or `receive_only`.
    #[allow(deprecated)]
    pub fn mode(mut self, mode: RelayMode) -> Self {
        self.config.direction = legacy_direction_for_client_mode(mode);
        self.config.client_roles = mode.roles();
        self.config.mode = mode;
        self
    }

    pub fn direction_generation(mut self, generation: u64) -> Self {
        self.config.direction_generation = generation;
        self.config.mode_generation = generation;
        self
    }

    pub fn codec(mut self, codec: CodecKind) -> Self {
        self.config.codec = codec;
        self
    }

    /// Preferred transport link (default: auto-select the best available).
    pub fn transport(mut self, transport: TransportPreference) -> Self {
        self.config.transport = transport;
        self
    }

    /// Audio format used for capture/playback PCM passed to the client, and
    /// the format negotiated with the host.
    ///
    /// See [`RelayHostBuilder::audio`]: this sets both the wire and the local
    /// geometry, because the PCM an SDK caller hands over is in exactly one
    /// format and the engine has to be told which.
    pub fn audio(mut self, sample_rate: u32, channels: u16, frame_ms: u16) -> Self {
        self.config.sample_rate = sample_rate;
        self.config.channels = channels;
        self.config.frame_ms = frame_ms;
        self.config.local_sample_rate = sample_rate;
        self.config.local_channels = channels;
        self
    }

    /// Override *only* the negotiated wire geometry. See
    /// [`RelayHostBuilder::wire_audio`].
    pub fn wire_audio(mut self, sample_rate: u32, channels: u16) -> Self {
        self.config.sample_rate = sample_rate;
        self.config.channels = channels;
        self
    }

    /// Override *only* the local application geometry. See
    /// [`RelayHostBuilder::local_audio`].
    pub fn local_audio(mut self, sample_rate: u32, channels: u16) -> Self {
        self.config.local_sample_rate = sample_rate;
        self.config.local_channels = channels;
        self
    }

    pub fn build(self) -> RelayResult<RelayClientPrepared> {
        validate_audio(
            self.config.sample_rate,
            self.config.channels,
            self.config.frame_ms,
        )?;
        validate_audio(
            self.config.local_sample_rate,
            self.config.local_channels,
            self.config.frame_ms,
        )?;
        Ok(RelayClientPrepared {
            config: self.config,
        })
    }
}

impl Default for RelayClientBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// A validated client that has not connected yet.
/// A client configuration ready to connect. Cloneable for the same reason as
/// [`RelayHostPrepared`]: a refused or timed-out connect leaves the caller
/// able to try again.
#[derive(Clone)]
pub struct RelayClientPrepared {
    config: EngineConfig,
}

impl RelayClientPrepared {
    /// Connect to a host. Blocks until the handshake completes or fails. The
    /// caller owns the PIN lifetime; the SDK does not persist or regenerate it.
    pub fn connect(self, target: &str, pin: &str) -> RelayResult<RelayClient> {
        let addr = resolve(target)?;
        let roles = self.config.client_roles;
        let engine = RelayEngine::start(self.config)?;
        let handle = engine.handle();
        let session = handle.connect(addr, pin, roles);

        // Wait for the handshake outcome so callers get synchronous errors.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        let mut trusted_peer = None;
        loop {
            for event in handle.events() {
                match event {
                    RelayEvent::TrustedPeerAvailable {
                        peer_id, secret, ..
                    } => {
                        trusted_peer = Some(TrustedPeer { peer_id, secret });
                    }
                    RelayEvent::SessionEstablished { id, peer, .. } if id == session => {
                        return Ok(RelayClient {
                            _engine: engine,
                            handle,
                            session,
                            host_name: peer.name,
                            trusted_peer,
                        });
                    }
                    RelayEvent::SessionLost { id, reason } if id == session => {
                        return Err(RelayError::Engine(format!(
                            "connection to {addr} failed: {reason}"
                        )));
                    }
                    _ => {}
                }
            }
            if std::time::Instant::now() > deadline {
                return Err(RelayError::Engine("connection timed out".into()));
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// Connect without a PIN using a credential obtained from an earlier
    /// explicit pairing with the same stable host identity.
    pub fn connect_trusted(
        self,
        target: &str,
        peer_id: &str,
        secret: [u8; 32],
    ) -> RelayResult<RelayClient> {
        let addr = resolve(target)?;
        let roles = self.config.client_roles;
        let engine = RelayEngine::start(self.config)?;
        let handle = engine.handle();
        let session = handle.connect_trusted(addr, peer_id, secret, roles);
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            for event in handle.events() {
                match event {
                    RelayEvent::SessionEstablished { id, peer, .. } if id == session => {
                        return Ok(RelayClient {
                            _engine: engine,
                            handle,
                            session,
                            host_name: peer.name,
                            trusted_peer: None,
                        });
                    }
                    RelayEvent::SessionLost { id, reason } if id == session => {
                        return Err(RelayError::Engine(format!(
                            "trusted connection to {addr} failed: {reason}"
                        )));
                    }
                    _ => {}
                }
            }
            if Instant::now() > deadline {
                return Err(RelayError::Engine("trusted connection timed out".into()));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

/// A connected relay client.
pub struct RelayClient {
    _engine: RelayEngine,
    handle: RelayHandle,
    session: SessionId,
    host_name: String,
    trusted_peer: Option<TrustedPeer>,
}

impl RelayClient {
    pub fn session(&self) -> SessionId {
        self.session
    }

    pub fn host_name(&self) -> &str {
        &self.host_name
    }

    /// Credential created by an explicit PIN pairing, if enabled in the
    /// builder. Store it in owner-only application storage and pass it to a
    /// later trusted connect.
    pub fn trusted_peer(&self) -> Option<TrustedPeer> {
        self.trusted_peer.clone()
    }

    pub fn handle(&self) -> RelayHandle {
        self.handle.clone()
    }

    /// Send captured microphone audio to the host in `MobileToDesktop`.
    pub fn send_capture(&self, samples: &[f32]) {
        self.handle.push_capture(samples);
    }

    /// Emit local PCM to the peer. The client must have been built with
    /// [`RelayMode::Emitter`].
    pub fn emit(&self, samples: &[f32]) {
        self.send_capture(samples);
    }

    /// Realtime-safe variant of [`Self::send_capture`]. It returns `false`
    /// when the input exceeds [`MAX_REALTIME_QUANTUM_SAMPLES`], a realtime
    /// lock is busy, or no accepting session is available; it never reports a
    /// partial input acceptance.
    pub fn try_send_capture(&self, samples: &[f32]) -> bool {
        self.handle.try_push_capture(samples)
    }

    /// Take host audio for playback in `DesktopToMobile`.
    pub fn pull_playback(&self, out: &mut [f32]) -> usize {
        self.handle.pull_playback(out)
    }

    /// Receive peer PCM into `out`. The client must have been built with
    /// [`RelayMode::Receiver`].
    pub fn receive(&self, out: &mut [f32]) -> usize {
        self.pull_playback(out)
    }

    /// Realtime-safe variant of [`Self::pull_playback`]. It returns zero on a
    /// busy lock or no data, may return partial output, and caps production at
    /// [`MAX_REALTIME_QUANTUM_SAMPLES`] samples.
    pub fn try_pull_playback(&self, out: &mut [f32]) -> usize {
        self.handle.try_pull_playback(out)
    }

    pub fn events(&self) -> Vec<RelayEvent> {
        self.handle.events()
    }

    /// Disconnect from the host.
    pub fn disconnect(self) -> RelayResult<()> {
        self.handle.disconnect(self.session)
    }

    /// Propose a new direction for this authenticated session. The embedding
    /// must wait for `DirectionResolved` before replacing its audio worker.
    pub fn offer_direction(&self, direction: RelayDirection, generation: u64) -> RelayResult<()> {
        self.handle
            .offer_direction(self.session, direction, generation)
    }

    /// Propose the authoritative emitter identity for the authenticated
    /// session.
    pub fn offer_flow(&self, flow: RelayFlow, generation: u64) -> RelayResult<()> {
        self.handle.offer_flow(self.session, flow, generation)
    }

    /// Propose an Emitter/Receiver role for this local endpoint.
    pub fn offer_mode(&self, mode: RelayMode, generation: u64) -> RelayResult<()> {
        self.handle.offer_mode(self.session, mode, generation)
    }

    pub fn remove_trusted_peer(&self, peer_id: &str) -> RelayResult<()> {
        self.handle.remove_trusted_peer(peer_id)
    }

    pub fn trusted_peers(&self) -> Vec<TrustedPeer> {
        self.handle.trusted_peers()
    }
}

/// Browse the local network for relay hosts for up to `timeout`, returning
/// every host seen. Uses mDNS/DNS-SD (`_qpw-relay._udp`); hosts that do not
/// advertise (or networks that block multicast) simply yield an empty list,
/// so callers should keep manual `host:port` entry as a fallback.
pub fn discover_hosts(timeout: Duration) -> RelayResult<Vec<PeerInfo>> {
    let engine = RelayEngine::start(EngineConfig::default())?;
    let handle = engine.handle();
    handle.discovery_start()?;
    let deadline = Instant::now() + timeout;
    loop {
        // Drain events so PeerDiscovered entries land in the peer snapshot.
        for event in handle.events() {
            if let RelayEvent::Error { message } = event {
                eprintln!("relay discovery: {message}");
            }
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let peers = handle.discovered_peers();
    handle.discovery_stop();
    engine.shutdown();
    Ok(peers)
}

/// A standalone, long-lived browser for relay hosts on the local network.
///
/// Unlike [`discover_hosts`], which blocks for a fixed timeout and tears the
/// engine down, a browser stays open so an embedding UI can start and stop
/// browsing on demand and read the current peer snapshot whenever it redraws.
/// The underlying [`RelayEngine`] opens no control socket: browsing works
/// with no host running and no client connected.
pub struct RelayBrowser {
    engine: RelayEngine,
    handle: RelayHandle,
}

impl RelayBrowser {
    /// Create a browser. `device_name` identifies this browser in mDNS probes.
    pub fn start(device_name: impl Into<String>) -> RelayResult<Self> {
        let config = EngineConfig {
            device_name: device_name.into(),
            ..EngineConfig::default()
        };
        let engine = RelayEngine::start(config)?;
        let handle = engine.handle();
        Ok(Self { engine, handle })
    }

    /// Clone the lightweight engine handle. Platform registries can copy this
    /// under their registry mutex and perform discovery work after releasing
    /// that process-wide lock.
    pub fn handle(&self) -> RelayHandle {
        self.handle.clone()
    }

    /// Begin browsing `_qpw-relay._udp`. Idempotent.
    pub fn discovery_start(&self) -> RelayResult<()> {
        self.handle.discovery_start()
    }

    /// Stop browsing and clear transient peers. A subsequent start rebuilds the
    /// snapshot from live mDNS/USB results rather than exposing stale addresses.
    /// Idempotent.
    pub fn discovery_stop(&self) {
        self.handle.discovery_stop()
    }

    /// Snapshot of relay hosts discovered so far.
    pub fn peers(&self) -> Vec<PeerInfo> {
        self.handle.discovered_peers()
    }

    /// Drain pending events (`PeerDiscovered`, `PeerLost`, errors). UIs poll
    /// this to learn *why* the peer list changed.
    pub fn events(&self) -> Vec<RelayEvent> {
        self.handle.events()
    }

    /// Stop the underlying engine and consume the browser.
    pub fn shutdown(self) {
        self.engine.shutdown();
    }
}

fn resolve(target: &str) -> RelayResult<SocketAddr> {
    let target = target.trim();
    if target.is_empty() {
        return Err(RelayError::Engine("relay target cannot be empty".into()));
    }
    if let Ok(address) = target.parse::<SocketAddr>() {
        if address.port() == 0 {
            return Err(RelayError::Engine(format!(
                "relay target has an invalid control port: {target:?}"
            )));
        }
        return Ok(address);
    }
    if target.starts_with('[') {
        let end = target.find(']').ok_or_else(|| {
            RelayError::Engine(format!(
                "relay target must use [ipv6]:port syntax: {target:?}"
            ))
        })?;
        if target.get(end + 1..end + 2) != Some(":") {
            return Err(RelayError::Engine(format!(
                "relay target is missing a control port: {target:?}"
            )));
        }
    } else if target.matches(':').count() != 1 {
        return Err(RelayError::Engine(format!(
            "relay target must be host:port: {target:?}"
        )));
    }
    target
        .rsplit_once(':')
        .and_then(|(_, port)| port.parse::<u16>().ok())
        .filter(|port| *port != 0)
        .ok_or_else(|| {
            RelayError::Engine(format!(
                "relay target has an invalid control port: {target:?}"
            ))
        })?;
    target
        .to_socket_addrs()
        .map_err(RelayError::Io)?
        .next()
        .ok_or_else(|| RelayError::Engine(format!("could not resolve host address {target:?}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_missing_or_zero_control_ports() {
        assert!(resolve("127.0.0.1").is_err());
        assert!(resolve("127.0.0.1:0").is_err());
        assert!(resolve("[::1]").is_err());
        assert!(resolve("[::1]:0").is_err());
        assert!(resolve("").is_err());
    }

    #[test]
    fn accepts_ipv4_and_ipv6_control_targets() {
        assert_eq!(resolve("127.0.0.1:48123").unwrap().port(), 48123);
        assert_eq!(resolve("[::1]:48123").unwrap().port(), 48123);
    }

    #[test]
    fn sdk_keeps_ephemeral_port_as_an_explicit_opt_in() {
        assert_eq!(RelayHostBuilder::new().config.port, 0);
        assert_eq!(RelayHostBuilder::new().port(48123).config.port, 48123);
        assert_eq!(RelayHostBuilder::new().port(0).config.port, 0);
    }

    #[test]
    #[allow(deprecated)]
    fn direction_builders_keep_one_way_roles_and_the_generation_together() {
        for direction in [
            RelayDirection::MobileToDesktop,
            RelayDirection::DesktopToMobile,
        ] {
            let host = RelayHostBuilder::new()
                .direction(direction)
                .direction_generation(7)
                .build()
                .expect("direction is always valid");
            assert_eq!(host.config.direction, direction);
            assert_eq!(host.config.direction_generation, 7);
            assert!(Roles::for_direction(direction).is_one_way());
            assert_eq!(Roles::for_direction(direction).direction(), Some(direction));

            let client = RelayClientBuilder::new()
                .direction(direction)
                .direction_generation(7)
                .build()
                .expect("direction is always valid");
            assert_eq!(client.config.direction, direction);
            assert_eq!(client.config.direction_generation, 7);
        }
    }

    #[test]
    fn host_builder_audio_settings_carry_into_a_running_host() {
        let host = RelayHostBuilder::new()
            .device_name("sdk-host-test")
            .pin("123456")
            .port(0)
            .audio(48_000, 1, 20)
            .build()
            .expect("builder")
            .start()
            .expect("host start");
        assert_ne!(host.port(), 0);
        let status = host.status();
        assert!(status.host_active);
        assert!(status.sessions.is_empty());
    }

    #[test]
    fn browser_starts_empty_and_tolerates_unmulticastable_envs() {
        let browser = RelayBrowser::start("sdk-browser-test").expect("browser");
        assert!(browser.peers().is_empty());
        // Browsing needs mDNS multicast; sandboxes may refuse the socket.
        // Either outcome is fine here — this test pins the API shape.
        if browser.discovery_start().is_ok() {
            let _ = browser.events();
            browser.discovery_stop();
        }
        browser.shutdown();
    }

    #[test]
    fn audio_sets_both_wire_and_local_geometry() {
        // The bug this pins: `.audio()` used to move only the wire half, so
        // an Android caller pushing 48 kHz interleaved stereo had it read as
        // 48 kHz mono — every other sample landing in the wrong frame.
        let prepared = RelayHostBuilder::new()
            .pin("123456")
            .audio(48_000, 2, 10)
            .build()
            .expect("builder");
        assert_eq!(prepared.config.sample_rate, 48_000);
        assert_eq!(prepared.config.channels, 2);
        assert_eq!(prepared.config.frame_ms, 10);
        assert_eq!(prepared.config.local_sample_rate, 48_000);
        assert_eq!(prepared.config.local_channels, 2);

        let prepared = RelayClientBuilder::new()
            .audio(48_000, 2, 10)
            .build()
            .expect("builder");
        assert_eq!(prepared.config.local_channels, 2);
        assert_eq!(prepared.config.local_sample_rate, 48_000);
    }

    #[test]
    fn audio_at_sixteen_kilohertz_mono_sets_both_geometries() {
        let prepared = RelayClientBuilder::new()
            .audio(16_000, 1, 20)
            .build()
            .expect("builder");
        assert_eq!(prepared.config.sample_rate, 16_000);
        assert_eq!(prepared.config.channels, 1);
        assert_eq!(prepared.config.frame_ms, 20);
        assert_eq!(prepared.config.local_sample_rate, 16_000);
        assert_eq!(prepared.config.local_channels, 1);
    }

    #[test]
    fn wire_and_local_audio_can_be_split_deliberately() {
        let prepared = RelayHostBuilder::new()
            .pin("123456")
            .audio(48_000, 2, 20)
            .wire_audio(16_000, 1)
            .build()
            .expect("builder");
        assert_eq!(prepared.config.sample_rate, 16_000);
        assert_eq!(prepared.config.channels, 1);
        assert_eq!(prepared.config.local_sample_rate, 48_000);
        assert_eq!(prepared.config.local_channels, 2);

        let prepared = RelayClientBuilder::new()
            .audio(16_000, 1, 20)
            .local_audio(48_000, 2)
            .build()
            .expect("builder");
        assert_eq!(prepared.config.sample_rate, 16_000);
        assert_eq!(prepared.config.local_channels, 2);
    }

    #[test]
    fn builders_reject_unsupported_audio_geometry_at_build() {
        // Failing here rather than mid-handshake is the point: a client would
        // otherwise get as far as a completed PAKE before learning that 44.1
        // kHz is not negotiable.
        for (rate, channels, frame_ms) in [
            (44_100u32, 1u16, 10u16),
            (48_000, 3, 10),
            (48_000, 0, 10),
            (48_000, 1, 7),
            (0, 1, 10),
        ] {
            let host = RelayHostBuilder::new()
                .pin("123456")
                .audio(rate, channels, frame_ms)
                .build();
            assert!(
                matches!(host, Err(RelayError::Config(_))),
                "host builder accepted {rate} Hz / {channels} ch / {frame_ms} ms"
            );
            let client = RelayClientBuilder::new()
                .audio(rate, channels, frame_ms)
                .build();
            assert!(
                matches!(client, Err(RelayError::Config(_))),
                "client builder accepted {rate} Hz / {channels} ch / {frame_ms} ms"
            );
        }
    }

    #[test]
    fn builders_accept_every_negotiable_audio_geometry() {
        for rate in SAMPLE_RATES_HZ {
            for channels in [1u16, 2] {
                for frame_ms in FRAME_DURATIONS_MS {
                    assert!(
                        RelayHostBuilder::new()
                            .pin("123456")
                            .audio(rate, channels, frame_ms)
                            .build()
                            .is_ok(),
                        "rejected {rate} Hz / {channels} ch / {frame_ms} ms"
                    );
                }
            }
        }
    }

    #[test]
    fn a_split_geometry_is_validated_on_both_halves() {
        let host = RelayHostBuilder::new()
            .pin("123456")
            .audio(48_000, 2, 10)
            .wire_audio(44_100, 1)
            .build();
        assert!(matches!(host, Err(RelayError::Config(_))));

        let client = RelayClientBuilder::new()
            .audio(48_000, 2, 10)
            .local_audio(48_000, 4)
            .build();
        assert!(matches!(client, Err(RelayError::Config(_))));
    }
}
