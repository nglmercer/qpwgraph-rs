//! Core graph types shared by every backend and presentation layer.

use serde::{Deserialize, Serialize};

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
    /// Daemon `media.class` (e.g. `Video/Source`). Empty when the backend
    /// does not report one; used for camera detection, never matching.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub media_class: String,
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
