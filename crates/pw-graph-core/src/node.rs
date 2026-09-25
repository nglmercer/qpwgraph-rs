//! Core graph types shared by every backend and presentation layer.

use super::endpoint::{Direction, EndpointMatchMode, EndpointSelector, NodeType, PortType};
use super::id::{LinkId, NodeId, PortId};
use super::identity::NodeIdentity;
use serde::{Deserialize, Serialize};

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
    /// Optional backend-provided icon reference supplied by the backend.
    /// On Linux this is an XDG icon name; on Windows it is either a full
    /// executable path (application sessions, icon extracted from the binary),
    /// a device icon resource reference (`path,-index`, from the MMDevice
    /// property store), or one of the `WINDOWS_*_ICON` sentinels below.
    /// This is presentation metadata rather than part of a node's durable
    /// routing identity, so the UI may resolve it against the user's current
    /// icon theme without changing selectors or patchbay files.
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

    /// Attach an optional backend-provided icon reference (an XDG icon name
    /// on Linux, an executable path, icon resource reference, or
    /// `WINDOWS_*_ICON` sentinel on Windows).
    pub fn with_icon_name(mut self, icon_name: impl Into<String>) -> Self {
        let icon_name = icon_name.into().trim().to_owned();
        self.icon_name = (!icon_name.is_empty()).then_some(icon_name);
        self
    }

    /// Whether this node is a camera source (V4L2 or libcamera). PipeWire
    /// marks camera nodes with `media.role=Camera` or
    /// `media.class=Video/Source`; the monitor name prefixes cover daemons
    /// that set neither. Screencast streams (`Stream/Output/Video`) and our
    /// own helper streams never reach the graph, so a bare `Video/Source`
    /// class is unambiguous here.
    pub fn is_camera(&self) -> bool {
        self.identity
            .media_role
            .as_deref()
            .is_some_and(|role| role.eq_ignore_ascii_case("camera"))
            || self
                .identity
                .media_class
                .eq_ignore_ascii_case("video/source")
            || self.name.starts_with("v4l2_input.")
            || self.name.starts_with("libcamera_input.")
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
