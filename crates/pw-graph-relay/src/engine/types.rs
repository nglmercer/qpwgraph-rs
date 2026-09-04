//! The values the engine reports outward: session and peer identity, the
//! event stream, and the status snapshots an embedder polls.

use super::*;

/// Identifier for one live relay session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SessionId(pub u64);

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "session-{}", self.0)
    }
}

/// A remote device we know about, discovered or connected.
#[derive(Clone, Debug, PartialEq)]
pub struct PeerInfo {
    /// Stable identity advertised by the peer. Socket addresses are
    /// deliberately not identity: a tethered peer may have a Wi-Fi address
    /// and a USB address at different times.
    pub id: String,
    pub name: String,
    pub kind: DeviceKind,
    pub addr: SocketAddr,
}

/// Events drained by the host application (typically once per UI frame).
#[derive(Clone, PartialEq)]
pub enum RelayEvent {
    HostStarted {
        port: u16,
    },
    HostStopped,
    /// A relay host appeared on the local network (mDNS browse).
    PeerDiscovered {
        peer: PeerInfo,
    },
    /// A previously discovered host went away.
    PeerLost {
        peer: PeerInfo,
    },
    /// A fresh PIN pairing produced a persistent credential. Embeddings may
    /// store it in owner-only application storage and use `connect_trusted`
    /// when the same peer is discovered again.
    TrustedPeerAvailable {
        peer_id: String,
        peer: PeerInfo,
        secret: [u8; 32],
    },
    /// A host embedding must durably persist the credential obtained through
    /// [`RelayHandle::trusted_enrollment_secret`] and then call
    /// [`RelayHandle::accept_trusted_enrollment`]. No credential is imported
    /// into the live engine and no TrustAccepted is sent before that call.
    TrustedPeerEnrollmentRequested {
        transaction_id: u64,
        peer_id: String,
        peer: PeerInfo,
    },
    SessionEstablished {
        id: SessionId,
        peer: PeerInfo,
        roles: Roles,
        codec: CodecKind,
    },
    /// The authenticated direction winner for a session. Both peers emit the
    /// same winner after an offer/ack exchange; embedders use it to perform
    /// their local two-phase endpoint switch.
    DirectionResolved {
        id: SessionId,
        generation: u64,
        direction: RelayDirection,
        winner_device_id: String,
    },
    /// Canonical authenticated flow winner. `flow.emitter_id` is authoritative
    /// and `mode` is this installation's derived local role.
    FlowResolved {
        id: SessionId,
        generation: u64,
        flow: RelayFlow,
        mode: RelayMode,
    },
    SessionLost {
        id: SessionId,
        reason: String,
    },
    /// Rough incoming level for a session, for metering. `rms` is 0..=1.
    AudioLevel {
        id: SessionId,
        rms: f32,
    },
    /// Non-fatal background error worth surfacing in the UI.
    Error {
        message: String,
    },
}

impl fmt::Debug for RelayEvent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TrustedPeerAvailable { peer_id, peer, .. } => formatter
                .debug_struct("TrustedPeerAvailable")
                .field("peer_id", peer_id)
                .field("peer", peer)
                .field("secret", &"<redacted>")
                .finish(),
            Self::TrustedPeerEnrollmentRequested {
                transaction_id,
                peer_id,
                peer,
            } => formatter
                .debug_struct("TrustedPeerEnrollmentRequested")
                .field("transaction_id", transaction_id)
                .field("peer_id", peer_id)
                .field("peer", peer)
                .finish(),
            Self::HostStarted { port } => formatter
                .debug_struct("HostStarted")
                .field("port", port)
                .finish(),
            Self::HostStopped => formatter.write_str("HostStopped"),
            Self::PeerDiscovered { peer } => formatter
                .debug_struct("PeerDiscovered")
                .field("peer", peer)
                .finish(),
            Self::PeerLost { peer } => formatter
                .debug_struct("PeerLost")
                .field("peer", peer)
                .finish(),
            Self::SessionEstablished {
                id,
                peer,
                roles,
                codec,
            } => formatter
                .debug_struct("SessionEstablished")
                .field("id", id)
                .field("peer", peer)
                .field("roles", roles)
                .field("codec", codec)
                .finish(),
            Self::DirectionResolved {
                id,
                generation,
                direction,
                winner_device_id,
            } => formatter
                .debug_struct("DirectionResolved")
                .field("id", id)
                .field("generation", generation)
                .field("direction", direction)
                .field("winner_device_id", winner_device_id)
                .finish(),
            Self::FlowResolved {
                id,
                generation,
                flow,
                mode,
            } => formatter
                .debug_struct("FlowResolved")
                .field("id", id)
                .field("generation", generation)
                .field("flow", flow)
                .field("mode", mode)
                .finish(),
            Self::SessionLost { id, reason } => formatter
                .debug_struct("SessionLost")
                .field("id", id)
                .field("reason", reason)
                .finish(),
            Self::AudioLevel { id, rms } => formatter
                .debug_struct("AudioLevel")
                .field("id", id)
                .field("rms", rms)
                .finish(),
            Self::Error { message } => formatter
                .debug_struct("Error")
                .field("message", message)
                .finish(),
        }
    }
}

/// Status snapshot for UI display.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionStatus {
    pub id: SessionId,
    pub peer: PeerInfo,
    pub roles: Roles,
    pub codec: CodecKind,
    /// True when this side sends audio in the session.
    pub sending: bool,
    /// True when this side receives audio in the session.
    pub receiving: bool,
    /// Carrier used by the session: `udp` for normal links or `adb-tcp` for
    /// ADB forwarding. This is diagnostic metadata, never an authorization
    /// signal.
    pub transport: String,
    /// Classified link used by the current peer address, when known.
    pub link: String,
    /// Local endpoint is not exposed until the transport has one to report.
    pub local_addr: Option<SocketAddr>,
    pub remote_addr: SocketAddr,
    pub control_state: String,
    pub audio_channel_state: String,
    pub trusted: bool,
    /// Canonical local role derived from the authenticated emitter identity.
    pub mode: Option<RelayMode>,
    /// Authoritative flow, when this session has completed generic
    /// negotiation. Legacy sessions expose `None` until they migrate.
    pub flow: Option<RelayFlow>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct EngineStatus {
    pub host_active: bool,
    pub host_port: Option<u16>,
    /// The exact IPv4 address selected for the active listener. `None` means
    /// the documented no-link fallback is listening on all IPv4 interfaces.
    pub host_addr: Option<Ipv4Addr>,
    pub sessions: Vec<SessionStatus>,
}

/// State of the control connection relevant to session resumption.
///
/// This is deliberately separate from the generation counter: the counter
/// identifies one set of control keys, while this state prevents a second
/// connection from taking over while the original control owner is still
/// active.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ControlState {
    Active,
    ResumeEligible { generation: u64 },
    Resuming { generation: u64 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ResumeGraceResult {
    /// The grace period expired without an in-flight resume.
    Expired,
    /// A different control generation already owns the session, or the
    /// session is otherwise no longer waiting for this grace period.
    Resumed,
    /// A resume challenge is being authenticated. The watcher must not turn
    /// this into an apparently active session with no control owner. The
    /// generation is the in-progress generation, not the stale generation
    /// owned by the control watcher that entered the grace period.
    InProgress { generation: u64 },
}
