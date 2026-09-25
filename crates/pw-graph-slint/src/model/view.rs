//! The projected shapes the UI reads: a node, its port groups, a link, and
//! the whole-graph snapshot the Slint models are rebuilt from.

use super::*;

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PortGroupView {
    pub(crate) pin_id: i32,
    pub(crate) ports: Vec<PortId>,
    pub(crate) label: String,
    pub(crate) direction: Direction,
    pub(crate) port_type: PortType,
    pub(crate) color: [u8; 4],
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct NodeView {
    pub(crate) id: i32,
    pub(crate) node_id: NodeId,
    pub(crate) title: String,
    pub(crate) node_type: NodeType,
    pub(crate) icon_name: Option<String>,
    pub(crate) position: [f32; 2],
    pub(crate) width: f32,
    pub(crate) height: f32,
    pub(crate) selected: bool,
    pub(crate) collapsed: bool,
    pub(crate) thumbnail: bool,
    pub(crate) font_scale: f32,
    pub(crate) appearance: NodeAppearance,
    pub(crate) has_audio_controls: bool,
    /// Whether the node has any audio panel, including a meter-only panel.
    pub(crate) has_audio_panel: bool,
    /// Whether the card shows the video action block (preview/stop). Video
    /// nodes never have audio ports in practice; when both panels would
    /// apply, the audio panel wins and this stays false.
    pub(crate) has_video_panel: bool,
    /// The video block offers a live preview of this node.
    pub(crate) video_preview: bool,
    /// The video block offers to stop this node (the active capture).
    pub(crate) video_stop: bool,
    /// Whether the node is a camera source (V4L2 or libcamera).
    pub(crate) is_camera: bool,
    /// Whether this node''s backend can rewire it. Backend-wide `connect` is a
    /// union across children, so it is true on Windows because MIDI can route
    /// even though Core Audio cannot.
    pub(crate) connectable: bool,
    /// Audio state and per-node capability, both read from the backend. The
    /// UI keeps no copy of its own: whatever is here is what the backend last
    /// reported, and an unknown value stays unknown.
    pub(crate) audio: NodeBackendProfile,
    pub(crate) meter: MeterReading,
    pub(crate) ports: Vec<PortGroupView>,
}

/// Which video actions a node card offers. Capture nodes get preview plus
/// stop; filter instances get preview only; anything else gets no panel.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct VideoPanel {
    pub(crate) preview: bool,
    pub(crate) stop: bool,
}

/// What the owning backend says about one node.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct NodeBackendProfile {
    pub(crate) state: NodeAudioState,
    pub(crate) capabilities: NodeCapabilities,
    /// Whether this node''s backend can rewire it.
    pub(crate) connectable: bool,
}

/// Stand-in for a backend that supports everything, used where a test cares
/// about projection rather than about capability gating.
#[cfg(test)]
pub(crate) fn fully_capable_backend_profiles(
    graph: &Graph,
) -> BTreeMap<NodeId, NodeBackendProfile> {
    graph
        .nodes
        .keys()
        .map(|node_id| {
            (
                *node_id,
                NodeBackendProfile {
                    state: NodeAudioState::readable(1.0, false),
                    capabilities: NodeCapabilities::FULL,
                    connectable: true,
                },
            )
        })
        .collect()
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct LinkView {
    pub(crate) id: i32,
    pub(crate) link_id: LinkId,
    pub(crate) start_pin_id: i32,
    pub(crate) end_pin_id: i32,
    pub(crate) color: [u8; 4],
    pub(crate) selected: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct GraphSnapshot {
    pub(crate) nodes: Vec<NodeView>,
    pub(crate) links: Vec<LinkView>,
}
