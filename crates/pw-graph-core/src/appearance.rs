//! Core graph types shared by every backend and presentation layer.

use serde::{Deserialize, Serialize};

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

/// Fallback [`crate::Node::icon_name`] the Windows backend uses for a playback
/// endpoint when the MMDevice property store exposes no device icon path.
/// The UI resolves it to a stock system icon; it never participates in
/// routing identity or patchbay persistence.
pub const WINDOWS_ENDPOINT_RENDER_ICON: &str = "windows:endpoint-render";

/// Fallback [`crate::Node::icon_name`] the Windows backend uses for a capture
/// endpoint when the MMDevice property store exposes no device icon path.
/// See [`WINDOWS_ENDPOINT_RENDER_ICON`].
pub const WINDOWS_ENDPOINT_CAPTURE_ICON: &str = "windows:endpoint-capture";
