//! Core graph types shared by every backend and presentation layer.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use thiserror::Error;

/// User-facing appearance overrides for a node. The backend keeps the native
/// node identity and name; this state controls how the node is presented in
/// the canvas and can be persisted by the application.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct NodeAppearance {
    pub collapsed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<[u8; 4]>,
}

/// PipeWire node name of the relay's virtual capture device, presented to the
/// user as "Relay Microphone". Defined here so the backend that creates the
/// node, the UI that renames it, and the patchbay compatibility rules below
/// all agree on one spelling.
pub const RELAY_SOURCE_NODE_NAME: &str = "qpwgraph-rs.relay.source";
/// PipeWire node name of the relay's virtual playback device, presented as
/// "Relay Speaker".
pub const RELAY_SINK_NODE_NAME: &str = "qpwgraph-rs.relay.sink";
/// Role prefix the relay source node gives its ports (`capture_FL`, ...).
pub const RELAY_SOURCE_PORT_ROLE: &str = "capture";
/// Role prefix the relay sink node gives its ports (`playback_FL`, ...).
pub const RELAY_SINK_PORT_ROLE: &str = "playback";

/// The port name a pre-rename patchbay entry should resolve to, if it is one.
///
/// The relay filter ports were once named for their bare channel — `FL` and
/// `FR` — which left them as two loose pins with no base for the canvas to
/// group on. They are now role-prefixed like every other node's ports, but
/// patchbay files saved before that rename still carry the bare names and
/// would otherwise silently stop reconnecting.
///
/// This is deliberately narrow: it fires only for the two relay node names,
/// only for the two bare channel names, and only in the direction that node
/// actually has ports in. An unrelated device with `FL`/`FR` ports is never
/// rewritten, and nothing here affects how new patchbays are written.
pub fn legacy_relay_port_name(
    node_name: &str,
    port_name: &str,
    direction: Direction,
) -> Option<&'static str> {
    if !matches!(port_name, "FL" | "FR") {
        return None;
    }
    match (node_name, direction) {
        (RELAY_SOURCE_NODE_NAME, Direction::Source) => match port_name {
            "FL" => Some("capture_FL"),
            _ => Some("capture_FR"),
        },
        (RELAY_SINK_NODE_NAME, Direction::Sink) => match port_name {
            "FL" => Some("playback_FL"),
            _ => Some("playback_FR"),
        },
        _ => None,
    }
}

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
/// This is deliberately independent of [`NodeType`]. A node type describes
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

/// Durable and session-scoped metadata used to identify a node without
/// treating a PipeWire global object id as an application identity.
///
/// application_id is the preferred cross-restart identity. The remaining
/// application/process fields are useful fallbacks and diagnostics. serial
/// is deliberately only a current-session hint: PipeWire assigns it when the
/// object is created, so it must never be the only durable selector.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct NodeIdentity {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub application_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub application_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_binary: Option<String>,
    #[serde(default)]
    pub node_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_api: Option<String>,
    /// Current PipeWire client global id. It is intentionally diagnostic-only
    /// and is never serialized into a durable selector.
    #[serde(default, skip_serializing, skip_deserializing)]
    pub client_id: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object_serial: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effect_instance_id: Option<String>,
}

impl NodeIdentity {
    pub fn with_node_name(node_name: impl Into<String>) -> Self {
        Self {
            node_name: node_name.into(),
            ..Self::default()
        }
    }

    pub fn has_application_identity(&self) -> bool {
        self.application_id.is_some()
            || self.process_binary.is_some()
            || self.application_name.is_some()
    }

    pub fn is_empty(&self) -> bool {
        self.application_id.is_none()
            && self.application_name.is_none()
            && self.process_binary.is_none()
            && self.node_name.is_empty()
            && self.description.is_none()
            && self.media_role.is_none()
            && self.media_name.is_none()
            && self.client_name.is_none()
            && self.client_api.is_none()
            && self.client_id.is_none()
            && self.object_path.is_none()
            && self.object_serial.is_none()
            && self.effect_instance_id.is_none()
    }
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

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Node {
    pub id: NodeId,
    pub name: String,
    pub node_type: NodeType,
    /// Backend-provided identity that survives global-ID churn when possible.
    /// PipeWire exposes this as `object.serial`; Windows uses a stable hash of
    /// its native endpoint/session identifier. Demo and ALSA nodes leave it
    /// unset and are resolved by their names.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serial: Option<u64>,
    /// Stable identity assigned by the effect host. PipeWire global IDs are
    /// intentionally not used for effect persistence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effect_instance_id: Option<String>,
    /// Rich endpoint identity. The legacy serial/effect fields above remain
    /// readable for older graph/config snapshots and are mirrored into this
    /// value by matching helpers.
    #[serde(default, skip_serializing_if = "NodeIdentity::is_empty")]
    pub identity: NodeIdentity,
    /// Optional XDG icon name supplied by the backend. This is presentation
    /// metadata rather than part of a node's durable routing identity, so the
    /// UI may resolve it against the user's current icon theme without
    /// changing selectors or patchbay files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon_name: Option<String>,
    pub ports: Vec<PortId>,
    /// Canvas position in logical scene coordinates.
    pub position: [f32; 2],
}

impl Node {
    pub fn new(id: NodeId, name: impl Into<String>, node_type: NodeType) -> Self {
        let name = name.into();
        Self {
            id,
            name: name.clone(),
            node_type,
            serial: None,
            effect_instance_id: None,
            identity: NodeIdentity::with_node_name(name),
            icon_name: None,
            ports: Vec::new(),
            position: [0.0, 0.0],
        }
    }

    pub fn with_serial(mut self, serial: u64) -> Self {
        self.serial = Some(serial);
        self.identity.object_serial = Some(serial);
        self
    }

    pub fn with_effect_instance(mut self, instance_id: impl Into<String>) -> Self {
        let instance_id = instance_id.into();
        self.effect_instance_id = Some(instance_id.clone());
        self.identity.effect_instance_id = Some(instance_id);
        self
    }

    pub fn with_identity(mut self, identity: NodeIdentity) -> Self {
        self.serial = identity.object_serial;
        self.effect_instance_id = identity.effect_instance_id.clone();
        self.identity = identity;
        if self.identity.node_name.is_empty() {
            self.identity.node_name = self.name.clone();
        }
        self
    }

    /// Attach an optional backend-provided XDG icon name.
    pub fn with_icon_name(mut self, icon_name: impl Into<String>) -> Self {
        let icon_name = icon_name.into().trim().to_owned();
        self.icon_name = (!icon_name.is_empty()).then_some(icon_name);
        self
    }

    /// Return the current node identity while preserving compatibility with
    /// graph values deserialized before the identity field was introduced.
    pub fn matching_identity(&self) -> NodeIdentity {
        let mut identity = self.identity.clone();
        if identity.node_name.is_empty() {
            identity.node_name = self.name.clone();
        }
        if identity.object_serial.is_none() {
            identity.object_serial = self.serial;
        }
        if identity.effect_instance_id.is_none() {
            identity.effect_instance_id = self.effect_instance_id.clone();
        }
        identity
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Port {
    pub id: PortId,
    pub node_id: NodeId,
    pub name: String,
    /// Optional backend-provided channel position (for example `FL` or
    /// `FR`). Backends that do not expose channel metadata leave this unset,
    /// allowing presentation code to use a conservative name-based fallback.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    pub direction: Direction,
    pub port_type: PortType,
}

/// Stable description of a port used when PipeWire recreates a stream and
/// assigns it a new global ID. The numeric [`PortId`] remains useful for the
/// current graph, while this key is used by commands and patchbay operations
/// that can outlive one registry snapshot.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PortKey {
    pub node_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_serial: Option<u64>,
    pub node_type: NodeType,
    pub port_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    pub direction: Direction,
    pub port_type: PortType,
    /// Rich selector metadata. None means this is a legacy name/serial key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<NodeIdentity>,
    #[serde(default)]
    pub match_mode: EndpointMatchMode,
}

impl PortKey {
    pub fn selector(&self) -> EndpointSelector {
        EndpointSelector {
            node_type: self.node_type,
            identity: self.identity.clone().unwrap_or_else(|| NodeIdentity {
                node_name: self.node_name.clone(),
                object_serial: self.node_serial,
                ..NodeIdentity::default()
            }),
            port_name: self.port_name.clone(),
            channel: self.channel.clone(),
            direction: self.direction,
            port_type: self.port_type,
            match_mode: self.match_mode,
        }
    }
}

impl Port {
    pub fn new(
        id: PortId,
        node_id: NodeId,
        name: impl Into<String>,
        direction: Direction,
        port_type: PortType,
    ) -> Self {
        Self {
            id,
            node_id,
            name: name.into(),
            channel: None,
            direction,
            port_type,
        }
    }

    /// Attach an optional backend-provided channel position to this port.
    pub fn with_channel(mut self, channel: impl Into<String>) -> Self {
        self.channel = Some(channel.into());
        self
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Link {
    pub id: LinkId,
    pub output_port: PortId,
    pub input_port: PortId,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct Graph {
    pub nodes: BTreeMap<NodeId, Node>,
    pub ports: BTreeMap<PortId, Port>,
    pub links: BTreeMap<LinkId, Link>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum GraphError {
    #[error("node {0} already exists")]
    DuplicateNode(NodeId),
    #[error("port {0} already exists")]
    DuplicatePort(PortId),
    #[error("link {0} already exists")]
    DuplicateLink(LinkId),
    #[error("node {0} does not exist")]
    MissingNode(NodeId),
    #[error("port {0} does not exist")]
    MissingPort(PortId),
    #[error("port {0} belongs to node {1}, not node {2}")]
    PortNodeMismatch(PortId, NodeId, NodeId),
    #[error("source port {0} must be a source")]
    NotSource(PortId),
    #[error("destination port {0} must be a sink")]
    NotSink(PortId),
    #[error("ports {0} and {1} are not compatible")]
    IncompatiblePorts(PortId, PortId),
    #[error("ports {0} and {1} are already linked")]
    DuplicateConnection(PortId, PortId),
    #[error("link {0} does not exist")]
    MissingLink(LinkId),
}

impl Graph {
    pub fn add_node(&mut self, node: Node) -> Result<(), GraphError> {
        if self.nodes.contains_key(&node.id) {
            return Err(GraphError::DuplicateNode(node.id));
        }
        self.nodes.insert(node.id, node);
        Ok(())
    }

    pub fn add_port(&mut self, port: Port) -> Result<(), GraphError> {
        let node = self
            .nodes
            .get_mut(&port.node_id)
            .ok_or(GraphError::MissingNode(port.node_id))?;
        if self.ports.contains_key(&port.id) {
            return Err(GraphError::DuplicatePort(port.id));
        }
        // A node may arrive already listing this port -- merging two graphs
        // clones whole nodes and then re-adds their ports. Listing the same id
        // twice draws the port twice, gives it two pins, and lets the second
        // (phantom) pin capture the link that belongs to the first.
        if !node.ports.contains(&port.id) {
            node.ports.push(port.id);
        }
        self.ports.insert(port.id, port);
        Ok(())
    }

    pub fn remove_link(&mut self, link_id: LinkId) -> Result<Link, GraphError> {
        self.links
            .remove(&link_id)
            .ok_or(GraphError::MissingLink(link_id))
    }

    pub fn link(&self, link_id: LinkId) -> Option<&Link> {
        self.links.get(&link_id)
    }

    pub fn port(&self, port_id: PortId) -> Option<&Port> {
        self.ports.get(&port_id)
    }

    pub fn port_key(&self, port_id: PortId) -> Option<PortKey> {
        let port = self.port(port_id)?;
        let node = self.node(port.node_id)?;
        Some(PortKey {
            node_name: node.name.clone(),
            node_serial: node.serial,
            node_type: node.node_type,
            port_name: port.name.clone(),
            channel: port.channel.clone(),
            direction: port.direction,
            port_type: port.port_type,
            identity: Some(node.matching_identity()),
            match_mode: EndpointMatchMode::Instance,
        })
    }

    /// Resolve a stable port key against the current registry snapshot.
    /// Serial is preferred, but a name fallback is intentional: a playback
    /// stream often receives a new serial when it is resumed.
    pub fn resolve_port_key(&self, key: &PortKey) -> Option<PortId> {
        // A patchbay saved before the relay ports were role-prefixed names
        // them `FL`/`FR`; accept that one rewrite so those files keep
        // reconnecting. See [`legacy_relay_port_name`] for its scope.
        self.resolve_endpoint(&key.selector()).port_id()
    }

    /// Resolve a stable key while retaining an ambiguity explanation for
    /// patchbay and diagnostic callers.
    pub fn resolve_port_key_result(&self, key: &PortKey) -> EndpointResolution {
        self.resolve_endpoint(&key.selector())
    }

    /// Explain the current selector result for a support/debug report. The
    /// resolver itself stays typed and compact; this control-plane helper
    /// turns the selected identity tier into a human-readable reason without
    /// exposing a numeric PipeWire id as if it were durable identity.
    pub fn endpoint_resolution_explanation(&self, selector: &EndpointSelector) -> String {
        let basis = endpoint_identity_basis(selector);
        match self.resolve_endpoint(selector) {
            EndpointResolution::Exact(id) => format!("matched port {id} via {basis}"),
            EndpointResolution::UniqueFallback(id) => {
                format!("matched port {id} via fallback {basis}")
            }
            EndpointResolution::Ambiguous(ids) => {
                format!("ambiguous ({}) via {basis}: {ids:?}", ids.len())
            }
            EndpointResolution::Missing => format!("missing via {basis}"),
        }
    }

    /// Resolve a typed selector against the current registry snapshot.
    ///
    /// Scores express identity confidence, not a preference for a numeric
    /// object id. Equal best scores are returned as Ambiguous so a recreated
    /// application stream cannot be connected to the wrong same-named node.
    pub fn resolve_endpoint(&self, selector: &EndpointSelector) -> EndpointResolution {
        let mut candidates: Vec<(u16, PortId)> = self
            .ports
            .values()
            .filter(|port| {
                let exact_name = port.name == selector.port_name
                    || legacy_relay_port_name(
                        &selector.identity.node_name,
                        &selector.port_name,
                        selector.direction,
                    )
                    .is_some_and(|name| port.name == name);
                // A selector with application identity can describe a stream
                // channel rather than a particular PipeWire port spelling.
                // This lets a recreated browser/Electron stream resolve when
                // its port name changes, while the channel and direction
                // filters below still prevent a left/right swap. A
                // name-pattern selector remains port-name specific.
                let has_application_identity = selector.identity.application_id.is_some()
                    || selector.identity.process_binary.is_some()
                    || selector.identity.application_name.is_some();
                let application_channel =
                    !matches!(selector.match_mode, EndpointMatchMode::NamePattern)
                        && has_application_identity
                        && selector.channel.is_some()
                        && selector.channel.as_ref() == port.channel.as_ref();
                exact_name || application_channel
            })
            .filter(|port| port.direction == selector.direction)
            .filter(|port| {
                port.port_type == selector.port_type
                    || port.port_type == PortType::Unknown
                    || selector.port_type == PortType::Unknown
            })
            .filter_map(|port| {
                let node = self.node(port.node_id)?;
                if selector.node_type != NodeType::Unknown && node.node_type != selector.node_type {
                    return None;
                }
                let has_application_identity = selector.identity.application_id.is_some()
                    || selector.identity.process_binary.is_some()
                    || selector.identity.application_name.is_some();
                let application_channel =
                    !matches!(selector.match_mode, EndpointMatchMode::NamePattern)
                        && has_application_identity
                        && selector.channel.is_some()
                        && selector.channel.as_ref() == port.channel.as_ref();
                // The compatibility relay rewrite must remain tied to the
                // relay node named by the saved selector.
                if port.name != selector.port_name
                    && node.name != selector.identity.node_name
                    && node.matching_identity().node_name != selector.identity.node_name
                    && !application_channel
                {
                    return None;
                }
                if let (Some(expected), Some(actual)) =
                    (selector.channel.as_ref(), port.channel.as_ref())
                {
                    if expected != actual {
                        return None;
                    }
                }
                let score = endpoint_match_score(node, selector)?;
                Some((score, port.id))
            })
            .collect();

        if candidates.is_empty() {
            return EndpointResolution::Missing;
        }
        candidates.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
        let best_score = candidates[0].0;
        let best: Vec<_> = candidates
            .iter()
            .take_while(|(score, _)| *score == best_score)
            .map(|(_, id)| *id)
            .collect();
        if best.len() > 1 {
            return EndpointResolution::Ambiguous(best);
        }
        if best_score >= 800 {
            EndpointResolution::Exact(best[0])
        } else {
            EndpointResolution::UniqueFallback(best[0])
        }
    }

    pub fn find_link_by_keys(&self, output: &PortKey, input: &PortKey) -> Option<Link> {
        let output_id = self.resolve_port_key(output)?;
        let input_id = self.resolve_port_key(input)?;
        self.links
            .values()
            .find(|link| link.output_port == output_id && link.input_port == input_id)
            .cloned()
    }

    pub fn node(&self, node_id: NodeId) -> Option<&Node> {
        self.nodes.get(&node_id)
    }

    pub fn add_link(
        &mut self,
        link_id: LinkId,
        output_port: PortId,
        input_port: PortId,
    ) -> Result<Link, GraphError> {
        if self.links.contains_key(&link_id) {
            return Err(GraphError::DuplicateLink(link_id));
        }
        let output = self
            .ports
            .get(&output_port)
            .ok_or(GraphError::MissingPort(output_port))?;
        let input = self
            .ports
            .get(&input_port)
            .ok_or(GraphError::MissingPort(input_port))?;
        if !output.direction.is_source() {
            return Err(GraphError::NotSource(output_port));
        }
        if !input.direction.is_sink() {
            return Err(GraphError::NotSink(input_port));
        }
        if output.port_type != input.port_type {
            return Err(GraphError::IncompatiblePorts(output_port, input_port));
        }
        if self
            .links
            .values()
            .any(|link| link.output_port == output_port && link.input_port == input_port)
        {
            return Err(GraphError::DuplicateConnection(output_port, input_port));
        }
        let link = Link {
            id: link_id,
            output_port,
            input_port,
        };
        self.links.insert(link_id, link.clone());
        Ok(link)
    }

    /// Insert a link reported by a backend snapshot. Backends may know about
    /// legacy or partially-described links that cannot be revalidated locally.
    pub fn insert_existing_link(&mut self, link: Link) -> Result<(), GraphError> {
        if self.links.contains_key(&link.id) {
            return Err(GraphError::DuplicateLink(link.id));
        }
        if !self.ports.contains_key(&link.output_port) {
            return Err(GraphError::MissingPort(link.output_port));
        }
        if !self.ports.contains_key(&link.input_port) {
            return Err(GraphError::MissingPort(link.input_port));
        }
        self.links.insert(link.id, link);
        Ok(())
    }

    pub fn links_for_port(&self, port_id: PortId) -> impl Iterator<Item = &Link> {
        self.links
            .values()
            .filter(move |link| link.output_port == port_id || link.input_port == port_id)
    }

    /// Suggest a readable, deterministic layout for every node in the graph.
    ///
    /// Connected nodes are assigned to layers following the direction of
    /// their links, so sources appear before their sinks and multi-hop graphs
    /// spread across several columns. Unconnected nodes use their port role as
    /// a fallback layer. Ordering inside a layer is stable by media category,
    /// direction, display name, and numeric ID, which keeps repeated refreshes
    /// from shuffling the graph.
    pub fn default_node_positions(&self) -> BTreeMap<NodeId, [f32; 2]> {
        let mut incoming: BTreeMap<NodeId, usize> =
            self.nodes.keys().copied().map(|node| (node, 0)).collect();
        let mut outgoing: BTreeMap<NodeId, Vec<NodeId>> = self
            .nodes
            .keys()
            .copied()
            .map(|node| (node, Vec::new()))
            .collect();
        for link in self.links.values() {
            let (Some(output), Some(input)) =
                (self.port(link.output_port), self.port(link.input_port))
            else {
                continue;
            };
            if output.node_id == input.node_id || !self.nodes.contains_key(&output.node_id) {
                continue;
            }
            outgoing
                .entry(output.node_id)
                .or_default()
                .push(input.node_id);
            if let Some(count) = incoming.get_mut(&input.node_id) {
                *count += 1;
            }
        }
        for targets in outgoing.values_mut() {
            targets.sort_unstable();
            targets.dedup();
        }

        let node_limit = self.nodes.len().saturating_sub(1);
        let mut graph_layers: BTreeMap<NodeId, usize> =
            self.nodes.keys().copied().map(|node| (node, 0)).collect();
        let mut queue: std::collections::VecDeque<NodeId> = incoming
            .iter()
            .filter_map(|(node, count)| (*count == 0).then_some(*node))
            .collect();
        if queue.is_empty() {
            queue.extend(self.nodes.keys().copied().take(1));
        }
        while let Some(node_id) = queue.pop_front() {
            let current_layer = graph_layers.get(&node_id).copied().unwrap_or_default();
            for target in outgoing.get(&node_id).into_iter().flatten() {
                let candidate = (current_layer + 1).min(node_limit);
                let target_layer = graph_layers.entry(*target).or_default();
                if candidate > *target_layer {
                    *target_layer = candidate;
                    queue.push_back(*target);
                }
            }
        }

        // A disconnected cycle has no zero-indegree root. Seed each remaining
        // component deterministically so it still receives a useful layer.
        for node_id in self.nodes.keys().copied() {
            if incoming.get(&node_id).copied().unwrap_or_default() > 0
                && graph_layers.get(&node_id).copied().unwrap_or_default() == 0
            {
                queue.push_back(node_id);
                while let Some(current) = queue.pop_front() {
                    let current_layer = graph_layers.get(&current).copied().unwrap_or_default();
                    for target in outgoing.get(&current).into_iter().flatten() {
                        let candidate = (current_layer + 1).min(node_limit);
                        let target_layer = graph_layers.entry(*target).or_default();
                        if candidate > *target_layer {
                            *target_layer = candidate;
                            queue.push_back(*target);
                        }
                    }
                }
            }
        }

        let mut layers: BTreeMap<usize, Vec<NodeId>> = BTreeMap::new();
        for node in self.nodes.values() {
            let graph_layer = graph_layers.get(&node.id).copied().unwrap_or_default();
            let role_layer = match self.node_layout_role(node) {
                0 => 0,
                1 => 1,
                2 => 2,
                _ => 0,
            };
            let layer = if graph_layer == 0 {
                role_layer
            } else {
                graph_layer
            };
            layers.entry(layer).or_default().push(node.id);
        }

        for nodes in layers.values_mut() {
            nodes.sort_by(|left, right| {
                let left_node = self.nodes.get(left).expect("layout node exists");
                let right_node = self.nodes.get(right).expect("layout node exists");
                self.node_media_category(left_node)
                    .cmp(&self.node_media_category(right_node))
                    .then_with(|| {
                        self.node_layout_role(left_node)
                            .cmp(&self.node_layout_role(right_node))
                    })
                    .then_with(|| {
                        left_node
                            .name
                            .to_ascii_lowercase()
                            .cmp(&right_node.name.to_ascii_lowercase())
                    })
                    .then_with(|| left.cmp(right))
            });
        }

        let mut positions = BTreeMap::new();
        for (layer, nodes) in layers {
            let mut top = 40.0;
            for node_id in nodes {
                let node = self.nodes.get(&node_id).expect("layout node exists");
                positions.insert(node_id, [40.0 + layer as f32 * 360.0, top]);
                let height = (34.0 + 14.0 + node.ports.len() as f32 * 25.0).max(62.0);
                top += height + 70.0;
            }
        }
        positions
    }

    fn node_media_category(&self, node: &Node) -> u8 {
        let mut has_audio = false;
        let mut has_video = false;
        let mut has_midi = false;
        for port_id in &node.ports {
            match self.port(*port_id).map(|port| port.port_type) {
                Some(PortType::Audio) => has_audio = true,
                Some(PortType::Video) => has_video = true,
                Some(PortType::MidiJack | PortType::MidiAlsa) => has_midi = true,
                _ => {}
            }
        }
        if has_audio {
            0
        } else if has_video {
            1
        } else if has_midi {
            2
        } else {
            3
        }
    }

    fn node_layout_role(&self, node: &Node) -> u8 {
        let mut has_source = false;
        let mut has_sink = false;
        for port_id in &node.ports {
            match self.port(*port_id).map(|port| port.direction) {
                Some(Direction::Source) => has_source = true,
                Some(Direction::Sink) => has_sink = true,
                None => {}
            }
        }
        match (has_source, has_sink) {
            (true, false) => 0,
            (false, true) => 2,
            _ => 1,
        }
    }
}

fn endpoint_match_score(node: &Node, selector: &EndpointSelector) -> Option<u16> {
    let actual = node.matching_identity();
    let expected = &selector.identity;

    if let Some(effect_instance_id) = expected.effect_instance_id.as_ref() {
        return (actual.effect_instance_id.as_ref() == Some(effect_instance_id)).then_some(1_000);
    }

    if let Some(application_id) = expected.application_id.as_ref() {
        if actual.application_id.as_ref() != Some(application_id) {
            return None;
        }
        let mut score = 800;
        if matches!(selector.match_mode, EndpointMatchMode::Instance) {
            if !expected.node_name.is_empty() && actual.node_name == expected.node_name {
                score += 70;
            }
            if expected.media_role.is_some() && expected.media_role == actual.media_role {
                score += 20;
            }
            // object.serial is only a same-session refinement after a
            // durable application identity matched; it is never sufficient
            // on its own.
            if expected.object_serial.is_some() && expected.object_serial == actual.object_serial {
                score += 10;
            }
        }
        return Some(score);
    }

    let has_process_fallback =
        expected.process_binary.is_some() || expected.application_name.is_some();
    if has_process_fallback {
        if expected.process_binary.is_some() && expected.process_binary != actual.process_binary {
            return None;
        }
        if expected.application_name.is_some()
            && expected.application_name != actual.application_name
        {
            return None;
        }
        let mut score = 620;
        if expected.media_role.is_some() && expected.media_role == actual.media_role {
            score += 30;
        }
        if matches!(selector.match_mode, EndpointMatchMode::Instance)
            && !expected.node_name.is_empty()
            && actual.node_name == expected.node_name
        {
            score += 30;
        }
        return Some(score);
    }

    // A PipeWire object.serial is intentionally not accepted as a
    // cross-restart application identity. It remains a useful current-session
    // hint for legacy Windows/native endpoint keys, and only after the node
    // type/name still agrees for PipeWire.
    if expected.object_serial.is_some()
        && expected.object_serial == actual.object_serial
        && (selector.node_type != NodeType::PipeWire
            || expected.node_name.is_empty()
            || expected.node_name == actual.node_name
            || expected.node_name == node.name)
    {
        return Some(600);
    }

    if expected.node_name.is_empty() {
        return Some(100);
    }
    let name_matches = match selector.match_mode {
        EndpointMatchMode::NamePattern => {
            let pattern = expected
                .node_name
                .strip_suffix('*')
                .unwrap_or(&expected.node_name);
            node.name.starts_with(pattern) || actual.node_name.starts_with(pattern)
        }
        EndpointMatchMode::Instance | EndpointMatchMode::Application => {
            node.name == expected.node_name || actual.node_name == expected.node_name
        }
    };
    name_matches.then_some(450)
}

fn endpoint_identity_basis(selector: &EndpointSelector) -> &'static str {
    let identity = &selector.identity;
    if identity.effect_instance_id.is_some() {
        "effect instance id"
    } else if identity.application_id.is_some() {
        "application.id"
    } else if identity.process_binary.is_some() || identity.application_name.is_some() {
        "process binary/application name"
    } else if identity.object_serial.is_some() {
        "object.serial session hint"
    } else if matches!(selector.match_mode, EndpointMatchMode::NamePattern) {
        "name pattern"
    } else if identity.node_name.is_empty() {
        "unqualified selector"
    } else {
        "legacy node name + port name"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Merging two graphs clones whole nodes -- ports vec included -- and then
    /// re-adds every port. Listing an id twice gives the port two rows and two
    /// pins on the card, and the phantom second pin steals the link that
    /// belongs to the real one, so edges are drawn to the wrong place.
    #[test]
    fn re_adding_a_port_a_node_already_lists_does_not_duplicate_it() {
        let source = graph();
        let mut merged = Graph::default();
        for node in source.nodes.values().cloned() {
            merged.add_node(node).unwrap();
        }
        for port in source.ports.values().cloned() {
            merged.add_port(port).unwrap();
        }

        for node in merged.nodes.values() {
            let mut unique = node.ports.clone();
            unique.sort();
            unique.dedup();
            assert_eq!(
                node.ports.len(),
                unique.len(),
                "node {:?} lists a port more than once: {:?}",
                node.name,
                node.ports
            );
        }
        assert_eq!(merged.nodes[&NodeId(1)].ports, vec![PortId(1)]);
        assert_eq!(merged.ports.len(), source.ports.len());
    }

    fn graph() -> Graph {
        let mut graph = Graph::default();
        graph
            .add_node(Node::new(NodeId(1), "Source", NodeType::PipeWire))
            .unwrap();
        graph
            .add_node(Node::new(NodeId(2), "Sink", NodeType::PipeWire))
            .unwrap();
        graph
            .add_port(Port::new(
                PortId(1),
                NodeId(1),
                "out",
                Direction::Source,
                PortType::Audio,
            ))
            .unwrap();
        graph
            .add_port(Port::new(
                PortId(2),
                NodeId(2),
                "in",
                Direction::Sink,
                PortType::Audio,
            ))
            .unwrap();
        graph
    }

    #[test]
    fn validates_and_removes_links() {
        let mut graph = graph();
        graph.add_link(LinkId(1), PortId(1), PortId(2)).unwrap();
        assert_eq!(graph.links.len(), 1);
        graph.remove_link(LinkId(1)).unwrap();
        assert!(graph.links.is_empty());
    }

    #[test]
    fn rejects_wrong_direction() {
        let mut graph = graph();
        let error = graph.add_link(LinkId(1), PortId(2), PortId(1)).unwrap_err();
        assert_eq!(error, GraphError::NotSource(PortId(2)));
    }

    #[test]
    fn stable_serial_resolves_a_renamed_windows_endpoint() {
        let node_id = NodeId(encode_backend_id(BackendNamespace::WindowsAudio, 1));
        let port_id = PortId(encode_backend_id(BackendNamespace::WindowsAudio, 2));
        let mut graph = Graph::default();
        graph
            .add_node(
                Node::new(
                    node_id,
                    "Speakers (old name)",
                    NodeType::WindowsAudioEndpoint,
                )
                .with_serial(0x1234),
            )
            .unwrap();
        graph
            .add_port(Port::new(
                port_id,
                node_id,
                "audio",
                Direction::Sink,
                PortType::Audio,
            ))
            .unwrap();

        let key = graph.port_key(port_id).unwrap();
        graph.nodes.get_mut(&node_id).unwrap().name = "Speakers (new name)".into();

        assert_eq!(graph.resolve_port_key(&key), Some(port_id));
    }

    fn application_graph(
        node_id: u64,
        port_id: u64,
        node_name: &str,
        application_id: &str,
        serial: u64,
        port_name: &str,
        channel: Option<&str>,
    ) -> Graph {
        let mut graph = Graph::default();
        graph
            .add_node(
                Node::new(NodeId(node_id), node_name, NodeType::PipeWire).with_identity(
                    NodeIdentity {
                        application_id: Some(application_id.into()),
                        application_name: Some("Test application".into()),
                        process_binary: Some("test-app".into()),
                        node_name: node_name.into(),
                        media_role: Some("music".into()),
                        object_serial: Some(serial),
                        ..NodeIdentity::default()
                    },
                ),
            )
            .unwrap();
        let port = Port::new(
            PortId(port_id),
            NodeId(node_id),
            port_name,
            Direction::Source,
            PortType::Audio,
        );
        graph
            .add_port(channel.map_or(port.clone(), |channel| port.with_channel(channel)))
            .unwrap();
        graph
    }

    #[test]
    fn application_selector_survives_global_id_churn() {
        let old = application_graph(
            10,
            20,
            "Discord",
            "com.example.discord",
            100,
            "output",
            None,
        );
        let key = old.port_key(PortId(20)).unwrap();
        let current = application_graph(
            145,
            227,
            "Discord",
            "com.example.discord",
            800,
            "output",
            None,
        );

        assert_eq!(
            current.resolve_port_key_result(&key),
            EndpointResolution::Exact(PortId(227))
        );
    }

    #[test]
    fn application_identity_survives_serial_churn_and_node_rename() {
        let old = application_graph(
            10,
            20,
            "Discord playback",
            "com.example.discord",
            100,
            "output",
            None,
        );
        let key = old.port_key(PortId(20)).unwrap();
        let current = application_graph(
            300,
            455,
            "Discord stream",
            "com.example.discord",
            801,
            "output",
            None,
        );

        assert_eq!(
            current.resolve_port_key_result(&key),
            EndpointResolution::Exact(PortId(455))
        );
    }

    #[test]
    fn process_identity_can_follow_a_renamed_port_when_application_id_is_absent() {
        let mut old = application_graph(
            10,
            20,
            "Discord playback",
            "org.example.discord",
            100,
            "old-output",
            Some("FL"),
        );
        old.nodes
            .get_mut(&NodeId(10))
            .expect("old node exists")
            .identity
            .application_id = None;
        let key = old.port_key(PortId(20)).unwrap();

        let mut current = application_graph(
            300,
            455,
            "Discord recreated",
            "org.example.discord",
            801,
            "new-output",
            Some("FL"),
        );
        current
            .nodes
            .get_mut(&NodeId(300))
            .expect("current node exists")
            .identity
            .application_id = None;

        assert_eq!(
            current.resolve_port_key_result(&key),
            EndpointResolution::UniqueFallback(PortId(455))
        );
    }

    #[test]
    fn same_named_applications_do_not_cross_match() {
        let mut graph = Graph::default();
        for (node_id, port_id, application_id) in [(1, 10, "app.one"), (2, 20, "app.two")] {
            let node = Node::new(NodeId(node_id), "Same name", NodeType::PipeWire).with_identity(
                NodeIdentity {
                    application_id: Some(application_id.into()),
                    node_name: "Same name".into(),
                    ..NodeIdentity::default()
                },
            );
            graph.add_node(node).unwrap();
            graph
                .add_port(Port::new(
                    PortId(port_id),
                    NodeId(node_id),
                    "output",
                    Direction::Source,
                    PortType::Audio,
                ))
                .unwrap();
        }
        let key = graph.port_key(PortId(10)).unwrap();

        assert_eq!(graph.resolve_port_key(&key), Some(PortId(10)));
    }

    #[test]
    fn duplicate_same_application_streams_are_ambiguous() {
        let mut graph = Graph::default();
        for (node_id, port_id, serial) in [(1, 10, 1), (2, 20, 2)] {
            let node = Node::new(NodeId(node_id), "Browser stream", NodeType::PipeWire)
                .with_identity(NodeIdentity {
                    application_id: Some("org.example.browser".into()),
                    node_name: "Browser stream".into(),
                    media_role: Some("music".into()),
                    object_serial: Some(serial),
                    ..NodeIdentity::default()
                });
            graph.add_node(node).unwrap();
            graph
                .add_port(Port::new(
                    PortId(port_id),
                    NodeId(node_id),
                    "output",
                    Direction::Source,
                    PortType::Audio,
                ))
                .unwrap();
        }
        let mut selector = EndpointSelector {
            node_type: NodeType::PipeWire,
            identity: NodeIdentity {
                application_id: Some("org.example.browser".into()),
                node_name: "Browser stream".into(),
                media_role: Some("music".into()),
                ..NodeIdentity::default()
            },
            port_name: "output".into(),
            channel: None,
            direction: Direction::Source,
            port_type: PortType::Audio,
            match_mode: EndpointMatchMode::Instance,
        };

        assert_eq!(
            graph.resolve_endpoint(&selector),
            EndpointResolution::Ambiguous(vec![PortId(10), PortId(20)])
        );

        selector.match_mode = EndpointMatchMode::Application;
        assert_eq!(
            graph.resolve_endpoint(&selector),
            EndpointResolution::Ambiguous(vec![PortId(10), PortId(20)])
        );
    }

    #[test]
    fn stereo_channel_selector_cannot_reverse_left_and_right() {
        let mut graph = application_graph(
            1,
            10,
            "Stereo app",
            "org.example.stereo",
            1,
            "output",
            Some("FL"),
        );
        graph
            .add_port(
                Port::new(
                    PortId(11),
                    NodeId(1),
                    "output",
                    Direction::Source,
                    PortType::Audio,
                )
                .with_channel("FR"),
            )
            .unwrap();
        let left = graph.port_key(PortId(10)).unwrap();
        assert_eq!(graph.resolve_port_key(&left), Some(PortId(10)));
    }

    #[test]
    fn application_mode_can_follow_a_changed_port_name_by_channel() {
        let old = application_graph(
            10,
            20,
            "Browser",
            "org.example.browser",
            100,
            "old-output",
            Some("FL"),
        );
        let old_key = old.port_key(PortId(20)).unwrap();
        let mut selector = old_key.selector();
        selector.match_mode = EndpointMatchMode::Application;
        let current = application_graph(
            300,
            455,
            "Browser recreated",
            "org.example.browser",
            800,
            "new-output",
            Some("FL"),
        );

        assert_eq!(
            current.resolve_endpoint(&selector),
            EndpointResolution::Exact(PortId(455))
        );
    }

    #[test]
    fn legacy_name_key_still_resolves_without_numeric_identity() {
        let graph = application_graph(10, 20, "Legacy source", "", 0, "output", None);
        let key = PortKey {
            node_name: "Legacy source".into(),
            node_serial: None,
            node_type: NodeType::PipeWire,
            port_name: "output".into(),
            channel: None,
            direction: Direction::Source,
            port_type: PortType::Audio,
            identity: None,
            match_mode: EndpointMatchMode::Instance,
        };

        assert_eq!(
            graph.resolve_port_key_result(&key),
            EndpointResolution::UniqueFallback(PortId(20))
        );
    }

    /// Build a graph holding both relay virtual nodes with their current
    /// role-prefixed ports, plus an unrelated device that also has `FL`/`FR`.
    fn relay_graph() -> (Graph, Vec<(PortId, &'static str)>) {
        let mut graph = Graph::default();
        graph
            .add_node(Node::new(
                NodeId(1),
                RELAY_SOURCE_NODE_NAME,
                NodeType::PipeWire,
            ))
            .unwrap();
        graph
            .add_node(Node::new(
                NodeId(2),
                RELAY_SINK_NODE_NAME,
                NodeType::PipeWire,
            ))
            .unwrap();
        graph
            .add_node(Node::new(
                NodeId(3),
                "alsa_output.pci-0000_00",
                NodeType::PipeWire,
            ))
            .unwrap();

        let mut ports = Vec::new();
        for (id, node, name, channel, direction) in [
            (10u64, NodeId(1), "capture_FL", "FL", Direction::Source),
            (11, NodeId(1), "capture_FR", "FR", Direction::Source),
            (12, NodeId(2), "playback_FL", "FL", Direction::Sink),
            (13, NodeId(2), "playback_FR", "FR", Direction::Sink),
            // The unrelated card still uses bare channel port names.
            (14, NodeId(3), "FL", "FL", Direction::Sink),
            (15, NodeId(3), "FR", "FR", Direction::Sink),
        ] {
            graph
                .add_port(
                    Port::new(PortId(id), node, name, direction, PortType::Audio)
                        .with_channel(channel),
                )
                .unwrap();
            ports.push((PortId(id), name));
        }
        (graph, ports)
    }

    /// A patchbay key exactly as an older file would have stored it: the
    /// relay node name with the bare channel as the port name.
    fn legacy_relay_key(node_name: &str, port_name: &str, direction: Direction) -> PortKey {
        PortKey {
            node_name: node_name.into(),
            node_serial: None,
            node_type: NodeType::PipeWire,
            port_name: port_name.into(),
            channel: Some(port_name.into()),
            direction,
            port_type: PortType::Audio,
            identity: None,
            match_mode: EndpointMatchMode::Instance,
        }
    }

    #[test]
    fn current_relay_patchbay_keys_resolve() {
        let (graph, ports) = relay_graph();
        for (id, _) in &ports[..4] {
            let key = graph.port_key(*id).unwrap();
            assert_eq!(graph.resolve_port_key(&key), Some(*id));
        }
    }

    #[test]
    fn legacy_relay_source_keys_resolve_to_the_capture_ports() {
        // Regression: renaming the relay filter ports to `capture_*` silently
        // orphaned every relay connection in a patchbay saved before it.
        let (graph, _) = relay_graph();
        assert_eq!(
            graph.resolve_port_key(&legacy_relay_key(
                RELAY_SOURCE_NODE_NAME,
                "FL",
                Direction::Source
            )),
            Some(PortId(10))
        );
        assert_eq!(
            graph.resolve_port_key(&legacy_relay_key(
                RELAY_SOURCE_NODE_NAME,
                "FR",
                Direction::Source
            )),
            Some(PortId(11))
        );
    }

    #[test]
    fn legacy_relay_sink_keys_resolve_to_the_playback_ports() {
        let (graph, _) = relay_graph();
        assert_eq!(
            graph.resolve_port_key(&legacy_relay_key(
                RELAY_SINK_NODE_NAME,
                "FL",
                Direction::Sink
            )),
            Some(PortId(12))
        );
        assert_eq!(
            graph.resolve_port_key(&legacy_relay_key(
                RELAY_SINK_NODE_NAME,
                "FR",
                Direction::Sink
            )),
            Some(PortId(13))
        );
    }

    #[test]
    fn unrelated_devices_keep_their_own_fl_fr_ports() {
        // The compatibility rewrite must not make a normal card's `FL` pin
        // resolve to a relay port, nor stop resolving to itself.
        let (graph, _) = relay_graph();
        let key = legacy_relay_key("alsa_output.pci-0000_00", "FL", Direction::Sink);
        assert_eq!(graph.resolve_port_key(&key), Some(PortId(14)));

        // A relay key in the direction that node has no ports in is not
        // rewritten into the other relay node either.
        let wrong_direction = legacy_relay_key(RELAY_SOURCE_NODE_NAME, "FL", Direction::Sink);
        assert_eq!(graph.resolve_port_key(&wrong_direction), None);
    }

    #[test]
    fn saving_a_relay_port_still_writes_the_role_prefixed_name() {
        // The migration is read-only: new patchbays must keep the new names.
        let (graph, _) = relay_graph();
        assert_eq!(graph.port_key(PortId(10)).unwrap().port_name, "capture_FL");
        assert_eq!(graph.port_key(PortId(12)).unwrap().port_name, "playback_FL");
    }

    #[test]
    fn default_layout_groups_media_and_direction() {
        let mut graph = Graph::default();
        for (id, name) in [(1, "Audio source"), (2, "Audio sink"), (3, "MIDI source")] {
            graph
                .add_node(Node::new(NodeId(id), name, NodeType::PipeWire))
                .unwrap();
        }
        graph
            .add_port(Port::new(
                PortId(10),
                NodeId(1),
                "out",
                Direction::Source,
                PortType::Audio,
            ))
            .unwrap();
        graph
            .add_port(Port::new(
                PortId(11),
                NodeId(2),
                "in",
                Direction::Sink,
                PortType::Audio,
            ))
            .unwrap();
        graph
            .add_port(Port::new(
                PortId(12),
                NodeId(3),
                "out",
                Direction::Source,
                PortType::MidiJack,
            ))
            .unwrap();

        let positions = graph.default_node_positions();
        assert!(positions[&NodeId(1)][0] < positions[&NodeId(2)][0]);
        assert!(positions[&NodeId(1)][1] < positions[&NodeId(3)][1]);
    }

    #[test]
    fn default_layout_places_connected_hops_in_ordered_layers() {
        let mut graph = Graph::default();
        for (id, name) in [(1, "Source"), (2, "Mixer"), (3, "Sink")] {
            graph
                .add_node(Node::new(NodeId(id), name, NodeType::PipeWire))
                .unwrap();
        }
        for (id, node, name, direction) in [
            (10, 1, "out", Direction::Source),
            (11, 2, "in", Direction::Sink),
            (12, 2, "out", Direction::Source),
            (13, 3, "in", Direction::Sink),
        ] {
            graph
                .add_port(Port::new(
                    PortId(id),
                    NodeId(node),
                    name,
                    direction,
                    PortType::Audio,
                ))
                .unwrap();
        }
        graph.add_link(LinkId(20), PortId(10), PortId(11)).unwrap();
        graph.add_link(LinkId(21), PortId(12), PortId(13)).unwrap();

        let positions = graph.default_node_positions();
        assert!(positions[&NodeId(1)][0] < positions[&NodeId(2)][0]);
        assert!(positions[&NodeId(2)][0] < positions[&NodeId(3)][0]);
        assert_eq!(positions, graph.default_node_positions());
    }

    #[test]
    fn backend_ids_round_trip_each_public_namespace() {
        for (namespace, backend) in [
            (BackendNamespace::PipeWire, BackendKind::PipeWire),
            (BackendNamespace::AlsaMidi, BackendKind::AlsaMidi),
            (BackendNamespace::WindowsAudio, BackendKind::WindowsAudio),
            (BackendNamespace::WindowsMidi, BackendKind::WindowsMidi),
            (BackendNamespace::Demo, BackendKind::Demo),
        ] {
            let id = encode_backend_id(namespace, 0x1234_5678);
            assert_eq!(decode_backend_namespace(id), namespace);
            assert_eq!(decode_backend_local_id(id), 0x1234_5678);
            assert_eq!(namespace.backend_kind(), Some(backend));
        }
    }

    #[test]
    fn backend_helpers_classify_typed_graph_ids() {
        assert_eq!(
            backend_for_node(NodeId(encode_backend_id(BackendNamespace::PipeWire, 7,))),
            Some(BackendKind::PipeWire)
        );
        assert_eq!(
            backend_for_port(PortId(
                encode_backend_id(BackendNamespace::WindowsAudio, 8,)
            )),
            Some(BackendKind::WindowsAudio)
        );
        assert_eq!(
            backend_for_link(LinkId(encode_backend_id(BackendNamespace::AlsaMidi, 9,))),
            Some(BackendKind::AlsaMidi)
        );
        assert_eq!(backend_for_node(NodeId(42)), Some(BackendKind::PipeWire));
    }

    #[test]
    fn legacy_alsa_high_bit_ids_still_decode() {
        let legacy = (1_u64 << 63) | 42;
        assert_eq!(decode_backend_namespace(legacy), BackendNamespace::AlsaMidi);
        assert_eq!(decode_backend_local_id(legacy), 42);
        assert_eq!(
            backend_for_port(PortId(legacy)),
            Some(BackendKind::AlsaMidi)
        );
    }
}
