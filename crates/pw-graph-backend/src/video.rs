//! Linux video backend contracts: filters, screen capture, and previews.
//!
//! Like [`crate::api::EffectDriver`], video support is layered beside the
//! graph API: backends without video keep the default `Unsupported` answers.
//! Video lives in the same [`pw_graph_core::Graph`] as audio — ports are
//! typed [`pw_graph_core::PortType::Video`] and the existing compatibility
//! checks already forbid `Audio <-> Video` links.

use pw_graph_core::{NodeId, PortId};
use pw_graph_video::diagnostics::VideoCounterSnapshot;
use pw_graph_video::filters::FilterParams;
use pw_graph_video::format::VideoSpec;
use pw_graph_video::preview::{PreviewState, VideoPreview};

use crate::api::{BackendError, BackendResult};

/// Preview handle re-exported so downstream crates need only `pw-graph-backend`.
pub use pw_graph_video::preview::VideoPreview as VideoPreviewHandle;

/// Stable instance id for a video filter node. Like audio effect instance
/// ids, this is the persistence identity — never a PipeWire global id.
pub type VideoFilterInstanceId = String;

/// Which content a screen-capture session records.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ScreenCastSource {
    #[default]
    Monitor,
    Window,
    Virtual,
}

impl ScreenCastSource {
    pub const ALL: [Self; 3] = [Self::Monitor, Self::Window, Self::Virtual];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Monitor => "monitor",
            Self::Window => "window",
            Self::Virtual => "virtual",
        }
    }
}

/// Lifecycle of one portal capture session.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ScreenCastState {
    #[default]
    Idle,
    /// The portal permission dialog may be on screen; the user decides.
    Requesting,
    Active,
    CancelledByUser,
    SessionClosed,
    SourceGone,
    Failed(String),
}

impl ScreenCastState {
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Active)
    }

    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Idle
                | Self::CancelledByUser
                | Self::SessionClosed
                | Self::SourceGone
                | Self::Failed(_)
        )
    }
}

/// Request to start capturing a monitor, window, or virtual display.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScreenCastRequest {
    pub source: ScreenCastSource,
    /// Show the cursor in the captured stream when the compositor allows it.
    pub show_cursor: bool,
    /// Capture multiple sources in one session when supported.
    pub multiple: bool,
}

impl Default for ScreenCastRequest {
    fn default() -> Self {
        Self {
            source: ScreenCastSource::Monitor,
            show_cursor: true,
            multiple: false,
        }
    }
}

/// Observable status of the current capture session.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScreenCastStatus {
    pub state: ScreenCastState,
    pub source: Option<ScreenCastSource>,
    /// Transient PipeWire node id of the capture stream, if currently known.
    /// Never persisted: node ids change across restarts.
    pub pipewire_node_id: Option<u32>,
    /// Stable-ish PipeWire serial (`object.serial`) for the stream, preferred
    /// over the transient node id when resolving the stream in the graph.
    pub object_serial: Option<u64>,
    pub error: Option<String>,
}

/// Request to create a virtual/extended display through the portal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VirtualDisplayRequest {
    pub width: u32,
    pub height: u32,
    /// Refresh rate in Hz, when the portal accepts one.
    pub refresh_hz: u32,
}

impl Default for VirtualDisplayRequest {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            refresh_hz: 60,
        }
    }
}

/// Virtual-display support and lifecycle. Support is runtime-detected per
/// compositor and never assumed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VirtualDisplayStatus {
    /// `None` until probed, then the compositor's real answer.
    pub supported: Option<bool>,
    pub active: bool,
    pub error: Option<String>,
}

/// Creation parameters for a PipeWire-visible video filter node.
#[derive(Clone, Debug, PartialEq)]
pub struct VideoFilterRequest {
    pub instance_id: VideoFilterInstanceId,
    pub filter_id: String,
    pub params: FilterParams,
    pub position: [f32; 2],
}

/// Live video filter node: one graph node with a video input and output port.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VideoFilterInstance {
    pub instance_id: VideoFilterInstanceId,
    pub node_id: NodeId,
    pub input_port: PortId,
    pub output_port: PortId,
    pub filter_id: String,
}

/// Processing state of one video node for the UI.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum VideoNodeState {
    #[default]
    Idle,
    Live,
    Bypassed,
    Failed,
}

impl VideoNodeState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Live => "live",
            Self::Bypassed => "bypassed",
            Self::Failed => "failed",
        }
    }
}

/// Everything the UI shows for one video node: negotiated format, state, and
/// counters.
#[derive(Clone, Debug, PartialEq)]
pub struct VideoNodeInfo {
    pub node_id: NodeId,
    pub instance_id: Option<VideoFilterInstanceId>,
    pub spec: Option<VideoSpec>,
    pub state: VideoNodeState,
    pub counters: Option<VideoCounterSnapshot>,
    pub preview_state: PreviewState,
}

/// Video support layered beside [`crate::api::GraphDriver`].
///
/// Default methods report `Unsupported` so non-Linux backends and builds
/// without the `pipewire` feature keep working unchanged.
pub trait VideoDriver {
    /// Whether this backend can host video filter nodes.
    fn video_supported(&self) -> bool {
        false
    }

    fn create_video_filter(
        &mut self,
        _request: VideoFilterRequest,
    ) -> BackendResult<VideoFilterInstance> {
        Err(BackendError::unsupported(
            "video filters are not available for this backend",
        ))
    }

    fn remove_video_filter(&mut self, _instance_id: &str) -> BackendResult<()> {
        Err(BackendError::unsupported(
            "video filters are not available for this backend",
        ))
    }

    /// Pause or resume a filter's worker thread. A disabled filter keeps its
    /// graph node and links but stops processing; the preview holds its
    /// last frame.
    fn set_video_filter_enabled(
        &mut self,
        _instance_id: &str,
        _enabled: bool,
    ) -> BackendResult<()> {
        Err(BackendError::unsupported(
            "video filters are not available for this backend",
        ))
    }

    fn video_filters(&self) -> Vec<VideoFilterInstance> {
        Vec::new()
    }

    /// Diagnostics snapshot for one video node, if the backend tracks it.
    fn video_node_info(&self, node: NodeId) -> Option<VideoNodeInfo> {
        let _ = node;
        None
    }

    /// Preview handle for a filter instance. Clones share the stream; polling
    /// never touches the realtime thread.
    fn video_preview(&self, _instance_id: &str) -> Option<VideoPreview> {
        None
    }

    /// Preview handle for the active screen-capture stream, if the backend
    /// taps one.
    fn screen_cast_preview(&self) -> Option<VideoPreview> {
        None
    }

    fn start_screen_cast(
        &mut self,
        _request: ScreenCastRequest,
    ) -> BackendResult<ScreenCastStatus> {
        Err(BackendError::unsupported(
            "screen capture is only available on Linux with the screencast feature",
        ))
    }

    fn stop_screen_cast(&mut self) -> BackendResult<()> {
        Err(BackendError::unsupported(
            "screen capture is only available on Linux with the screencast feature",
        ))
    }

    fn screen_cast_status(&self) -> ScreenCastStatus {
        ScreenCastStatus::default()
    }

    fn create_virtual_display(
        &mut self,
        _request: VirtualDisplayRequest,
    ) -> BackendResult<VirtualDisplayStatus> {
        Err(BackendError::unsupported(
            "virtual displays are only available on Linux with the screencast feature",
        ))
    }

    fn stop_virtual_display(&mut self) -> BackendResult<()> {
        Err(BackendError::unsupported(
            "virtual displays are only available on Linux with the screencast feature",
        ))
    }

    fn virtual_display_status(&self) -> VirtualDisplayStatus {
        VirtualDisplayStatus::default()
    }
}

/// Validate a virtual-display request against the same limits as capture
/// specs. Compositor support is still runtime-detected separately.
pub fn validate_virtual_display(request: &VirtualDisplayRequest) -> BackendResult<()> {
    if request.width == 0
        || request.height == 0
        || request.width > pw_graph_video::MAX_VIDEO_WIDTH
        || request.height > pw_graph_video::MAX_VIDEO_HEIGHT
    {
        return Err(BackendError::Native(format!(
            "invalid virtual display size {}x{}",
            request.width, request.height
        )));
    }
    if request.refresh_hz == 0 || request.refresh_hz > pw_graph_video::MAX_VIDEO_FPS {
        return Err(BackendError::Native(format!(
            "invalid virtual display refresh rate {} Hz",
            request.refresh_hz
        )));
    }
    Ok(())
}
