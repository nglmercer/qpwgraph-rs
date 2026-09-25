//! Core graph types shared by every backend and presentation layer.

use serde::{Deserialize, Serialize};

macro_rules! id_type {
    ($name:ident) => {
        #[derive(
            Clone,
            Copy,
            Debug,
            Default,
            Deserialize,
            Eq,
            Hash,
            Ord,
            PartialEq,
            Serialize,
            PartialOrd,
        )]
        #[serde(transparent)]
        pub struct $name(pub u64);

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}", self.0)
            }
        }
    };
}

id_type!(NodeId);
id_type!(PortId);
id_type!(LinkId);

/// Identifies the native backend that owns a graph resource.
///
/// This is deliberately independent of [`crate::NodeType`]. A node type describes
/// how a resource is presented, while backend identity determines which driver
/// receives a mutation such as connect, disconnect, or volume control.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendKind {
    PipeWire,
    AlsaMidi,
    WindowsAudio,
    WindowsMidi,
    Demo,
}

impl BackendKind {
    /// Stable string used in diagnostics and future persisted metadata.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PipeWire => "pipewire",
            Self::AlsaMidi => "alsa_midi",
            Self::WindowsAudio => "windows_audio",
            Self::WindowsMidi => "windows_midi",
            Self::Demo => "demo",
        }
    }

    /// Numeric namespace reserved for IDs emitted by this backend.
    pub const fn namespace(self) -> BackendNamespace {
        match self {
            Self::PipeWire => BackendNamespace::PipeWire,
            Self::AlsaMidi => BackendNamespace::AlsaMidi,
            Self::WindowsAudio => BackendNamespace::WindowsAudio,
            Self::WindowsMidi => BackendNamespace::WindowsMidi,
            Self::Demo => BackendNamespace::Demo,
        }
    }
}

/// Numeric namespaces stored in the high byte of graph IDs.
///
/// The `Effect` namespace is reserved for a future separately-hosted effect
/// backend. PipeWire-hosted effect nodes use the PipeWire namespace because
/// the PipeWire driver owns their links and controls.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[repr(u8)]
#[serde(rename_all = "snake_case")]
pub enum BackendNamespace {
    #[default]
    Unknown = 0,
    PipeWire = 1,
    AlsaMidi = 2,
    WindowsAudio = 3,
    WindowsMidi = 4,
    Effect = 5,
    Demo = 6,
}

impl BackendNamespace {
    /// Convert a namespace to the backend that owns it, if any.
    pub const fn backend_kind(self) -> Option<BackendKind> {
        match self {
            Self::PipeWire => Some(BackendKind::PipeWire),
            Self::AlsaMidi => Some(BackendKind::AlsaMidi),
            Self::WindowsAudio => Some(BackendKind::WindowsAudio),
            Self::WindowsMidi => Some(BackendKind::WindowsMidi),
            Self::Demo => Some(BackendKind::Demo),
            Self::Unknown | Self::Effect => None,
        }
    }
}

/// Number of bits reserved for a backend namespace in a graph ID.
pub const BACKEND_SHIFT: u32 = 56;

/// Mask for the backend-local portion of a graph ID.
pub const LOCAL_ID_MASK: u64 = 0x00FF_FFFF_FFFF_FFFF;

const LEGACY_ALSA_ID_FLAG: u64 = 1_u64 << 63;

/// Encode a native ID in a backend namespace.
///
/// Native IDs are expected to fit in the lower 56 bits. Masking here keeps the
/// representation well-defined for callers while the debug assertion catches
/// an invalid native ID during development.
pub fn encode_backend_id(namespace: BackendNamespace, native_id: u64) -> u64 {
    debug_assert_eq!(native_id & !LOCAL_ID_MASK, 0);
    (u64::from(namespace as u8) << BACKEND_SHIFT) | (native_id & LOCAL_ID_MASK)
}

/// Decode the namespace byte from a graph ID.
///
/// IDs written by older releases used only the high bit for ALSA MIDI. That
/// representation is recognized here so an already-open legacy graph remains
/// routable while new IDs use the explicit byte-wide namespace layout.
pub fn decode_backend_namespace(id: u64) -> BackendNamespace {
    match (id >> BACKEND_SHIFT) as u8 {
        1 => BackendNamespace::PipeWire,
        2 => BackendNamespace::AlsaMidi,
        3 => BackendNamespace::WindowsAudio,
        4 => BackendNamespace::WindowsMidi,
        5 => BackendNamespace::Effect,
        6 => BackendNamespace::Demo,
        0x80 => BackendNamespace::AlsaMidi,
        _ => BackendNamespace::Unknown,
    }
}

/// Decode the backend-local portion of a graph ID.
pub fn decode_backend_local_id(id: u64) -> u64 {
    if (id >> BACKEND_SHIFT) as u8 == 0x80 {
        id & !LEGACY_ALSA_ID_FLAG
    } else {
        id & LOCAL_ID_MASK
    }
}

/// Return the backend namespace owner for a node ID.
pub fn backend_for_node(id: NodeId) -> Option<BackendKind> {
    backend_for_id(id.0)
}

/// Return the backend namespace owner for a port ID.
pub fn backend_for_port(id: PortId) -> Option<BackendKind> {
    backend_for_id(id.0)
}

/// Return the backend namespace owner for a link ID.
pub fn backend_for_link(id: LinkId) -> Option<BackendKind> {
    backend_for_id(id.0)
}

fn backend_for_id(id: u64) -> Option<BackendKind> {
    decode_backend_namespace(id)
        .backend_kind()
        // Before namespaces were introduced, PipeWire graph IDs were raw
        // non-zero native IDs. Keep those IDs routable for old callers and
        // in-memory patchbay state while every new native backend emits an
        // explicit namespace.
        .or_else(|| (id != 0 && id >> BACKEND_SHIFT == 0).then_some(BackendKind::PipeWire))
}
