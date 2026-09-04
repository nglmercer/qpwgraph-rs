//! The relay engine, split by what each part owns.
//!
//! | Module | Owns |
//! | --- | --- |
//! | [`handle`] | the public engine and the handle an embedder holds |
//! | [`inner`] | the shared interior every session thread holds |
//! | [`record`] | one live session's shared state and stop flag |
//! | [`config`] | how an engine is configured, and its identity |
//! | [`types`] | session/peer identity, events, status snapshots |
//! | [`trust`] | trusted peers and the enrollment transaction |
//! | [`limits`] | every bound an untrusted peer could push against |
//! | [`error`] | what the engine can refuse to do |

// `engine` sits beside the transport modules rather than above them, so its
// items are `pub(crate)`: the session threads in `crate::session` are siblings
// and reach them by path.
use crate::{
    discovery, pairing, resolve_direction_offers, resolve_flow_offers, session, usb_probe,
    AudioFormat, CodecKind, Converter, DeviceKind, DirectionAck, DirectionOffer, FlowAck,
    FlowOffer, LinkKind, Opener, PcmQueue, RelayDirection, RelayFlow, RelayMode, Roles, Sealer,
    TransportPreference,
};
use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use thiserror::Error;

pub(crate) mod config;
pub(crate) mod error;
pub(crate) mod handle;
pub(crate) mod inner;
pub(crate) mod limits;
pub(crate) mod record;
pub(crate) mod trust;
pub(crate) mod types;

#[cfg(test)]
mod tests;

pub(crate) use self::config::*;
pub(crate) use self::error::*;
pub(crate) use self::handle::*;
pub(crate) use self::inner::*;
pub(crate) use self::limits::*;
pub(crate) use self::record::*;
pub(crate) use self::trust::*;
pub(crate) use self::types::*;
