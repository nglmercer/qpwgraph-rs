//! `pw-graph-relay` — networked audio relay engine.
//!
//! This crate implements the PC side of an AudioRelay-style feature: audio
//! captured on a phone (or any peer) can be *emitted* to this machine, and
//! audio played here can be *received* and rendered by a peer. Transport runs
//! over the local network (Wi-Fi LAN, USB tethering, or Bluetooth PAN) with
//! Opus-compressed UDP audio and a JSON-over-TCP control channel.
//!
//! The crate is UI-free and PipeWire-free so it also compiles for Android.
//! [`RelayEngine`] owns all sockets and worker threads; a cheap, cloneable
//! [`RelayHandle`] exposes commands and audio push/pull endpoints.
//!
//! Wire protocol: `docs/relay-protocol.md`.

pub mod audio;
pub mod codec;
pub mod convert;
pub mod crypto;
pub mod discovery;
pub mod netlink;
pub mod pairing;
pub mod protocol;
pub mod qr;
pub mod usb_probe;

mod queue;
mod realtime;
mod session;

pub use codec::AudioFormat;
pub use convert::Converter;
pub use crypto::{Opener, Sealer};
pub use netlink::{LinkKind, LocalLink, TransportPreference};
pub use protocol::{
    is_supported_channels, is_supported_frame_ms, is_supported_sample_rate, normalize_frame_ms,
    resolve_direction_offers, resolve_flow_offers, CodecKind, DeviceKind, DirectionAck,
    DirectionOffer, FlowAck, FlowOffer, LocalRelayMode, RelayDirection, RelayFlow, RelayMode,
    Roles, FRAME_DURATIONS_MS, MAX_CHANNELS, MAX_SAMPLE_RATE_HZ, SAMPLE_RATES_HZ,
};
pub use queue::{PcmQueue, CAPTURE_DEPTH_FRAMES, DEFAULT_QUEUE_CAPACITY, PLAYBACK_DEPTH_FRAMES};

mod engine;

pub use self::engine::config::*;
pub use self::engine::error::*;
pub use self::engine::handle::*;
pub(crate) use self::engine::inner::*;
pub use self::engine::limits::*;
pub(crate) use self::engine::record::*;
pub use self::engine::trust::*;
pub use self::engine::types::*;
