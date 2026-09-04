//! Session establishment and audio worker threads.
//!
//! Threading model (deliberately simple for a handful of local peers):
//!
//! - one accept-loop thread while hosting, one thread per connected peer,
//! - one thread per outgoing connection attempt,
//! - per established session: the control thread keeps watching keepalives,
//!   plus one UDP receiver and, when this side transmits, one UDP sender.
//!
//! Every loop checks its session's stop flag and the engine's running flag,
//! so shutdown completes within roughly one socket timeout.
//!
//! The submodules follow that model rather than the wire protocol:
//!
//! | Module | Owns |
//! | --- | --- |
//! | [`transport`] | UDP/TCP audio slots, their binds, live socket migration |
//! | [`workers`] | Spawning per-session threads and reporting their startup |
//! | [`host`] | Control listener, accept loop, per-peer host threads |
//! | [`client`] | Outgoing attempts and post-authentication session setup |
//! | [`handshake`] | PIN/PAKE pairing, trusted hello, credential enrolment |
//! | [`resume`] | Challenge/proof, the host's grace period, reconnect targets |
//! | [`control`] | Control cipher, keepalive/watch loops, teardown |
//! | [`audio_worker`] | The RX and TX workers both transports converge on |
//!
//! This module keeps only what all of them share: the imports, the timing and
//! sizing constants, and the re-exports the rest of the crate reaches for.

use crate::audio::{
    announce_packet, seal_datagram, AudioHeader, AudioPacket, JitterBuffer, JitterPop,
};
use crate::codec::{make_decoder, make_encoder, AudioFormat};
use crate::convert::Converter;
use crate::crypto::{
    pake_start, resume_control_channel, resume_proof, verify_resume_proof, Opener, Sealer,
    SessionKeys, Side, RESUME_NONCE_LEN,
};
use crate::netlink;
use crate::protocol::{
    is_supported_frame_ms, read_frame, read_sealed_frame, write_frame, write_sealed_frame,
    CodecKind, ControlMessage, DeviceKind, DirectionAck, DirectionOffer, FlowAck, FlowOffer, Roles,
    PROTOCOL_VERSION,
};
use crate::realtime::{request_realtime_thread, tune_audio_socket};
use crate::{
    ControlState, DirectionNegotiation, DirectionResolution, EngineInner, FlowResolution, PeerInfo,
    RelayError, RelayEvent, RelayResult, ResumeGraceResult, SessionId, SessionRecord,
};
use pw_graph_utils::hex::{hex_decode, hex_encode};
use rand::RngCore;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use subtle::ConstantTimeEq;

mod audio_worker;
mod client;
mod control;
mod handshake;
mod host;
mod resume;
mod transport;
mod workers;

#[cfg(test)]
mod tests;

// Everything below lived in one 5,000-line file. The modules keep the same
// visibility they had there: `pub(super)` means "private to `session`", which
// is what a bare `fn` meant before the split.
use self::audio_worker::*;
pub(crate) use self::client::*;
pub(crate) use self::control::*;
use self::handshake::*;
pub(crate) use self::host::*;
use self::resume::*;
pub(crate) use self::transport::*;
use self::workers::*;

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(2);
const SESSION_TIMEOUT: Duration = Duration::from_secs(6);
/// How long a freshly paired client waits for the host embedding's
/// accept/decline of its enrollment: the host's full decision window
/// ([`crate::TRUST_ENROLLMENT_TIMEOUT`]) plus handshake slack for the decision
/// and acknowledgement to cross the wire.
const ENROLLMENT_ACK_TIMEOUT: Duration =
    Duration::from_secs(crate::TRUST_ENROLLMENT_TIMEOUT.as_secs() + HANDSHAKE_TIMEOUT.as_secs());
/// How long a host keeps a dropped session alive waiting for the client to
/// re-establish its control channel (link roaming, brief Wi-Fi outages).
const RESUME_GRACE: Duration = Duration::from_secs(15);
/// Client-side reconnect attempts before a session is declared lost.
const RESUME_ATTEMPTS: u32 = 3;
/// Initial buffering before playback starts, in frames. Two is the smallest
/// depth that still lets the sender's keyframe anchor the stream when its
/// first two datagrams arrive out of order; the buffer's reorder tolerance
/// adapts from there, and the receive queue's target depth bounds what the
/// priming delay can turn into. At the default 10 ms frame this is 20 ms.
const JITTER_DEPTH_FRAMES: usize = 2;
const MAX_DATAGRAM: usize = 8192;
/// How long the sender parks waiting for a complete frame. The condvar wakes
/// it as soon as one is ready, so this only bounds how long teardown takes to
/// be noticed.
const FRAME_WAIT_TIMEOUT: Duration = Duration::from_millis(250);
