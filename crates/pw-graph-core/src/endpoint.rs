//! Core graph types shared by every backend and presentation layer.

use super::id::PortId;
use super::identity::NodeIdentity;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub enum Direction {
    #[default]
    Source,
    Sink,
}

impl Direction {
    pub fn is_source(self) -> bool {
        matches!(self, Self::Source)
    }

    pub fn is_sink(self) -> bool {
        matches!(self, Self::Sink)
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub enum NodeType {
    #[default]
    PipeWire,
    Effect,
    Recorder,
    AlsaMidi,
    WindowsAudioEndpoint,
    WindowsAudioSession,
    WindowsMidi,
    Unknown,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub enum PortType {
    #[default]
    Audio,
    Video,
    MidiJack,
    MidiAlsa,
    Unknown,
}

/// Persistence semantics for a selector. Instance matching is specific to a
/// stream when the graph exposes enough metadata; application matching is
/// intentionally broader and may be used for deterministic fan-out.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EndpointMatchMode {
    #[default]
    Instance,
    Application,
    NamePattern,
}

/// A typed endpoint selector suitable for patchbay/config persistence.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EndpointSelector {
    pub node_type: NodeType,
    pub identity: NodeIdentity,
    pub port_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    pub direction: Direction,
    pub port_type: PortType,
    #[serde(default)]
    pub match_mode: EndpointMatchMode,
}

/// A resolver result that fails closed when equally good candidates exist.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EndpointResolution {
    Exact(PortId),
    UniqueFallback(PortId),
    Ambiguous(Vec<PortId>),
    Missing,
}

impl EndpointResolution {
    pub fn port_id(&self) -> Option<PortId> {
        match self {
            Self::Exact(id) | Self::UniqueFallback(id) => Some(*id),
            Self::Ambiguous(_) | Self::Missing => None,
        }
    }
}

impl PortType {
    pub fn color_hex(self) -> &'static str {
        match self {
            Self::Audio => "#57c785",
            Self::Video => "#4e9de6",
            Self::MidiJack => "#e35d6a",
            Self::MidiAlsa => "#a979d1",
            Self::Unknown => "#a5a5a5",
        }
    }
}
