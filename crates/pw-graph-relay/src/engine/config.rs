//! How an engine is configured, including the installation identity it
//! advertises.

use super::*;

/// Engine-wide configuration. Apply with [`RelayHandle::update_config`]
/// before starting a host; `connect` reads the audio parameters live.
#[derive(Clone, PartialEq)]
pub struct EngineConfig {
    /// Stable identity for this installation. It is advertised in discovery
    /// records and bound into trusted handshakes.
    pub device_id: String,
    /// Advertised device name.
    pub device_name: String,
    pub device_kind: DeviceKind,
    /// Pairing PIN. Hosts must set one before [`RelayHandle::host_start`].
    pub pin: String,
    /// TCP control port when hosting; 0 picks an ephemeral port.
    pub port: u16,
    pub codec: CodecKind,
    pub frame_ms: u16,
    pub sample_rate: u32,
    /// 1 (mono microphone) or 2 (stereo playback).
    pub channels: u16,
    /// Roles used when this engine connects to a host as a client.
    pub client_roles: Roles,
    /// Direction this installation proposes when a session is established or
    /// switched. Hosts use this directly; clients normally keep it aligned
    /// with [`Roles::direction`].
    pub direction: RelayDirection,
    /// Monotonic generation persisted by the embedding application. It lets a
    /// trusted reconnect carry a direction choice made while offline.
    pub direction_generation: u64,
    /// Preferred transport link (`auto` picks the best available).
    pub transport: TransportPreference,
    /// Sample rate of this machine's own audio endpoints. Sessions are
    /// converted to and from this rate, so a peer negotiating 16 kHz does not
    /// play back at three times the pitch.
    pub local_sample_rate: u32,
    /// Channel count of this machine's own audio endpoints.
    pub local_channels: u16,
    /// Local address the host listens on. When `None`, the best active
    /// relay-capable link selected by [`TransportPreference::Auto`] is used;
    /// only a machine with no usable link information falls back to every
    /// IPv4 interface.
    pub bind_addr: Option<Ipv4Addr>,
    /// Concurrent connections allowed to sit in the pairing handshake. Each
    /// costs a thread and a five-second read timeout before it has proven
    /// anything, so an unbounded count is a trivial resource-exhaustion path.
    pub max_pending_handshakes: usize,
    /// Established sessions a host will hold at once.
    pub max_sessions: usize,
    /// Trusted peer credentials imported by the embedding application.
    pub trusted_peers: Vec<TrustedPeer>,
    /// Generate a trusted credential after an explicit PIN pairing. This is
    /// enabled by default so a user who pairs once gets real cable
    /// auto-connect; embedders that want PIN-only operation can disable it.
    pub trust_new_peers: bool,
}

impl fmt::Debug for EngineConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EngineConfig")
            .field("device_id", &self.device_id)
            .field("device_name", &self.device_name)
            .field("device_kind", &self.device_kind)
            .field("pin", &"<redacted>")
            .field("port", &self.port)
            .field("codec", &self.codec)
            .field("frame_ms", &self.frame_ms)
            .field("sample_rate", &self.sample_rate)
            .field("channels", &self.channels)
            .field("client_roles", &self.client_roles)
            .field("direction", &self.direction)
            .field("direction_generation", &self.direction_generation)
            .field("transport", &self.transport)
            .field("local_sample_rate", &self.local_sample_rate)
            .field("local_channels", &self.local_channels)
            .field("bind_addr", &self.bind_addr)
            .field("max_pending_handshakes", &self.max_pending_handshakes)
            .field("max_sessions", &self.max_sessions)
            .field("trusted_peers", &self.trusted_peers)
            .field("trust_new_peers", &self.trust_new_peers)
            .finish()
    }
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            device_id: generate_device_id(),
            device_name: "qpwgraph-rs".into(),
            device_kind: DeviceKind::Linux,
            pin: String::new(),
            port: 0,
            codec: CodecKind::Opus,
            frame_ms: 10,
            sample_rate: 48_000,
            channels: 1,
            client_roles: Roles::emit_only(),
            direction: RelayDirection::MobileToDesktop,
            direction_generation: 0,
            transport: TransportPreference::Auto,
            local_sample_rate: 48_000,
            local_channels: 1,
            bind_addr: None,
            max_pending_handshakes: 8,
            max_sessions: 16,
            trusted_peers: Vec::new(),
            trust_new_peers: true,
        }
    }
}

/// Generate a durable-format installation identity for an embedding that
/// wants to persist it outside the relay config.
pub fn generate_device_id() -> String {
    let mut bytes = [0u8; 16];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut bytes);
    pw_graph_utils::hex::hex_encode(&bytes)
}

impl EngineConfig {
    /// This machine's own audio geometry, as a frame-less format.
    pub fn local_format(&self) -> AudioFormat {
        AudioFormat::new(self.local_sample_rate, self.local_channels, self.frame_ms)
    }
}
