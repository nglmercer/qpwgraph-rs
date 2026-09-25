//! Linux video UI actions: capture, virtual displays, filters, previews.
//!
//! Handlers run on the Slint event thread. Starting a portal capture blocks
//! that thread until the compositor dialog resolves — the dialog is modal and
//! immediate, and drivers are `!Send`, so an async ticket would add machinery
//! without changing what the user sees. Everything else here is non-blocking:
//! previews poll the newest frame on the 50 ms pump and never touch the
//! realtime thread.

use pw_graph_backend::video::{
    ScreenCastRequest, ScreenCastSource, ScreenCastState, VideoFilterRequest, VideoNodeInfo,
    VirtualDisplayRequest,
};
use pw_graph_core::NodeId;
use slint::{Image, Rgba8Pixel, SharedPixelBuffer, SharedString};

use super::app::Application;
use super::MainWindow;

/// Which stream a preview dialog shows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum PreviewTarget {
    #[default]
    None,
    Filter(String),
    Capture,
}

/// Runtime-only video UI state. Filter instances live in the backend; this
/// only tracks what the canvas dialogs show.
#[derive(Clone, Debug, Default)]
pub(crate) struct VideoUiState {
    pub(crate) preview: PreviewTarget,
    preview_sequence: u64,
    filter_sequence: u64,
}

impl VideoUiState {
    fn next_instance_id(&mut self, filter_id: &str) -> String {
        self.filter_sequence += 1;
        format!("{filter_id}-{}", self.filter_sequence)
    }
}

fn video_available(application: &Application) -> bool {
    application.source.video_supported()
}

fn require_video(application: &mut Application) -> bool {
    if video_available(application) {
        true
    } else {
        application.status = application.t("status.video_unavailable");
        false
    }
}

/// Start capturing a monitor or window. Blocks on the portal permission
/// dialog; see the module docs.
pub(crate) fn start_capture(application: &mut Application, source: ScreenCastSource) {
    if !require_video(application) {
        return;
    }
    if application.source.screen_cast_status().state.is_active() {
        application.status = application.t("status.capture_already_active");
        return;
    }
    application.status = application.t("status.capture_requesting");
    match application.source.start_screen_cast(ScreenCastRequest {
        source,
        show_cursor: true,
        multiple: false,
    }) {
        Ok(status) => {
            application.status = match &status.state {
                ScreenCastState::Active => application.t("status.capture_active"),
                ScreenCastState::CancelledByUser => application.t("status.capture_cancelled"),
                ScreenCastState::Failed(message) => {
                    application.tf("status.capture_failed", &[("error", message.clone())])
                }
                other => {
                    application.tf("status.capture_failed", &[("error", format!("{other:?}"))])
                }
            };
            if let Err(error) = application.source.refresh() {
                application.status = application.tf("status.refresh_failed", &[("error", error)]);
            }
        }
        Err(error) => {
            application.status = application.tf("status.capture_failed", &[("error", error)]);
        }
    }
}

pub(crate) fn stop_capture(application: &mut Application) {
    if !require_video(application) {
        return;
    }
    match application.source.stop_screen_cast() {
        Ok(()) => {
            application.status = application.t("status.capture_stopped");
            if application.video.preview == PreviewTarget::Capture {
                application.video.preview = PreviewTarget::None;
            }
            let _ = application.source.refresh();
        }
        Err(error) => {
            application.status = application.tf("status.capture_failed", &[("error", error)]);
        }
    }
}

/// Create a virtual display through the portal. `geometry` is `WIDTHxHEIGHT@HZ`;
/// anything unparseable falls back to 1920x1080@60.
pub(crate) fn create_virtual_display(application: &mut Application, geometry: Option<&str>) {
    if !require_video(application) {
        return;
    }
    let request = parse_virtual_geometry(geometry);
    application.status = application.tf(
        "status.virtual_creating",
        &[(
            "geometry",
            format!(
                "{}x{}@{}",
                request.width, request.height, request.refresh_hz
            ),
        )],
    );
    match application.source.create_virtual_display(request) {
        Ok(status) => {
            if status.active {
                application.status = application.t("status.virtual_active");
            } else if status.supported == Some(false) {
                application.status = application.t("status.virtual_unsupported");
            } else {
                application.status = application.tf(
                    "status.virtual_failed",
                    &[("error", status.error.unwrap_or_else(|| "?".into()))],
                );
            }
            let _ = application.source.refresh();
        }
        Err(error) => {
            application.status = application.tf("status.virtual_failed", &[("error", error)]);
        }
    }
}

pub(crate) fn stop_virtual_display(application: &mut Application) {
    if !require_video(application) {
        return;
    }
    match application.source.stop_virtual_display() {
        Ok(()) => {
            application.status = application.t("status.virtual_stopped");
            let _ = application.source.refresh();
        }
        Err(error) => {
            application.status = application.tf("status.virtual_failed", &[("error", error)]);
        }
    }
}

fn parse_virtual_geometry(geometry: Option<&str>) -> VirtualDisplayRequest {
    let Some(geometry) = geometry else {
        return VirtualDisplayRequest::default();
    };
    // `WIDTHxHEIGHT@HZ`, e.g. `2560x1440@60`.
    let (size, hz) = geometry.split_once('@').unwrap_or((geometry, "60"));
    let (width, height) = size.split_once(['x', 'X']).unwrap_or((size, ""));
    let request = VirtualDisplayRequest {
        width: width.parse().unwrap_or(1920),
        height: height.parse().unwrap_or(1080),
        refresh_hz: hz.parse().unwrap_or(60),
    };
    if pw_graph_backend::video::validate_virtual_display(&request).is_ok() {
        request
    } else {
        VirtualDisplayRequest::default()
    }
}

/// Add a filter. `spec` is `filter-id` plus optional `;crop=x,y,w,h` or
/// `;scale=WxH` parameters for the geometry filters.
pub(crate) fn add_video_filter(application: &mut Application, spec: &str) {
    if !require_video(application) {
        return;
    }
    if !application.source.capabilities().connect {
        application.status = application.t("status.connections_unavailable");
        return;
    }
    let (filter_id, params) = match parse_filter_spec(spec) {
        Ok(parsed) => parsed,
        Err(message) => {
            application.status =
                application.tf("status.video_filter_failed", &[("error", message)]);
            return;
        }
    };
    let instance_id = application.video.next_instance_id(&filter_id);
    let position = [260.0, 180.0];
    match application.source.create_video_filter(VideoFilterRequest {
        instance_id: instance_id.clone(),
        filter_id: filter_id.clone(),
        params,
        position,
    }) {
        Ok(instance) => {
            application.view.selected_nodes.clear();
            application.view.selected_nodes.insert(instance.node_id);
            application.status = application.tf(
                "status.video_filter_added",
                &[("name", format!("{filter_id} {instance_id}"))],
            );
            let _ = application.source.refresh();
        }
        Err(error) => {
            application.status = application.tf("status.video_filter_failed", &[("error", error)]);
        }
    }
}

fn parse_filter_spec(
    spec: &str,
) -> Result<(String, pw_graph_video::filters::FilterParams), String> {
    use pw_graph_video::filters::{CropRect, FilterParams, ScaleSize};

    let mut parts = spec.split(';');
    let filter_id = parts.next().unwrap_or("").trim().to_owned();
    if filter_id.is_empty() {
        return Err("missing filter id".into());
    }
    let mut params = FilterParams::default();
    for part in parts {
        let part = part.trim();
        if let Some(crop) = part.strip_prefix("crop=") {
            let numbers: Vec<u32> = crop
                .split(',')
                .filter_map(|n| n.trim().parse().ok())
                .collect();
            if numbers.len() != 4 {
                return Err(format!("bad crop rectangle {crop:?}, want x,y,w,h"));
            }
            params.crop = Some(CropRect {
                x: numbers[0],
                y: numbers[1],
                width: numbers[2],
                height: numbers[3],
            });
        } else if let Some(scale) = part.strip_prefix("scale=") {
            let (width, height) = scale.split_once(['x', 'X']).unwrap_or((scale, ""));
            params.scale = Some(ScaleSize {
                width: width
                    .trim()
                    .parse()
                    .map_err(|_| format!("bad scale size {scale:?}"))?,
                height: height
                    .trim()
                    .parse()
                    .map_err(|_| format!("bad scale size {scale:?}"))?,
            });
        } else if !part.is_empty() {
            return Err(format!("unknown filter parameter {part:?}"));
        }
    }
    Ok((filter_id, params))
}

fn selected_video_instance(application: &Application) -> Option<String> {
    let selected: Vec<NodeId> = application.view.selected_nodes.iter().copied().collect();
    if selected.len() != 1 {
        return None;
    }
    application
        .source
        .video_filters()
        .iter()
        .find(|filter| filter.node_id == selected[0])
        .map(|filter| filter.instance_id.clone())
}

pub(crate) fn remove_selected_video_filter(application: &mut Application) {
    if !require_video(application) {
        return;
    }
    let Some(instance_id) = selected_video_instance(application) else {
        application.status = application.t("status.video_select_filter");
        return;
    };
    match application.source.remove_video_filter(&instance_id) {
        Ok(()) => {
            if application.video.preview == PreviewTarget::Filter(instance_id.clone()) {
                application.video.preview = PreviewTarget::None;
            }
            application.status =
                application.tf("status.video_filter_removed", &[("name", instance_id)]);
            let _ = application.source.refresh();
        }
        Err(error) => {
            application.status = application.tf("status.video_filter_failed", &[("error", error)]);
        }
    }
}

pub(crate) fn toggle_selected_video_filter(application: &mut Application) {
    if !require_video(application) {
        return;
    }
    let Some(instance_id) = selected_video_instance(application) else {
        application.status = application.t("status.video_select_filter");
        return;
    };
    // Enable state is not tracked locally; enabling an already-enabled
    // filter is a no-op, so toggle via disable-then-decide is avoidable:
    // query node info and flip Bypassed <-> Live/Idle.
    let disable = application
        .source
        .video_node_info(
            application
                .source
                .video_filters()
                .iter()
                .find(|filter| filter.instance_id == instance_id)
                .map(|filter| filter.node_id)
                .unwrap_or(NodeId(0)),
        )
        .is_none_or(|info| info.state != pw_graph_backend::video::VideoNodeState::Bypassed);
    match application
        .source
        .set_video_filter_enabled(&instance_id, !disable)
    {
        Ok(()) => {
            application.status = application.t(if disable {
                "status.video_filter_disabled"
            } else {
                "status.video_filter_enabled"
            });
        }
        Err(error) => {
            application.status = application.tf("status.video_filter_failed", &[("error", error)]);
        }
    }
}

/// Open the preview dialog for the selected filter, `capture`, or an
/// explicit instance id.
pub(crate) fn open_preview(application: &mut Application, target: Option<&str>) {
    if !require_video(application) {
        return;
    }
    let preview = match target {
        Some("capture") | None if application.video.preview == PreviewTarget::None => {
            // No explicit target: prefer the selected filter, else capture.
            match selected_video_instance(application) {
                Some(instance_id) => PreviewTarget::Filter(instance_id),
                None => PreviewTarget::Capture,
            }
        }
        Some("capture") => PreviewTarget::Capture,
        Some(instance_id) => PreviewTarget::Filter(instance_id.to_owned()),
        None => application.video.preview.clone(),
    };
    if preview == PreviewTarget::None {
        return;
    }
    application.video.preview = preview;
    application.video.preview_sequence = 0;
}

pub(crate) fn close_preview(window: &MainWindow, application: &mut Application) {
    application.video.preview = PreviewTarget::None;
    window.set_show_video_preview(false);
    window.set_video_preview_image(Image::default());
}

/// Pump tick: refresh the open preview dialog from the newest frame.
/// Conversion runs on the UI thread at pump cadence and only when a new
/// frame arrived; capture and processing never wait for it.
pub(crate) fn poll_video_preview(window: &MainWindow, application: &mut Application) {
    if application.video.preview == PreviewTarget::None {
        if window.get_show_video_preview() {
            window.set_show_video_preview(false);
        }
        return;
    }
    let preview = match &application.video.preview {
        PreviewTarget::Filter(instance_id) => application.source.video_preview(instance_id),
        PreviewTarget::Capture => application.source.screen_cast_preview(),
        PreviewTarget::None => None,
    };
    let Some(preview) = preview else {
        window.set_video_preview_status(SharedString::from(
            application.t("video.preview_unavailable"),
        ));
        window.set_show_video_preview(true);
        return;
    };
    window.set_show_video_preview(true);
    let title = match &application.video.preview {
        PreviewTarget::Filter(instance_id) => application.tf(
            "video.preview_filter_title",
            &[("name", instance_id.clone())],
        ),
        PreviewTarget::Capture => application.t("video.preview_capture_title"),
        PreviewTarget::None => String::new(),
    };
    window.set_video_preview_title(SharedString::from(title));
    let state_text = preview_state_text(application, &preview);
    window.set_video_preview_status(SharedString::from(state_text));
    let Some(frame) = preview.latest() else {
        return;
    };
    if frame.sequence() == application.video.preview_sequence && frame.sequence() != 0 {
        return;
    }
    application.video.preview_sequence = frame.sequence();
    let Some(rgba) = pw_graph_video::preview::video_frame_to_rgba_bytes(&frame) else {
        return;
    };
    let (width, height) = (frame.width(), frame.height());
    // Cap the displayed resolution so a 4K/8K stream cannot stall the UI
    // thread with a multi-megapixel conversion every tick.
    let (pixels, width, height) = downscale_rgba_for_preview(&rgba, width, height);
    let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(width, height);
    buffer.make_mut_slice().copy_from_slice(&pixels);
    window.set_video_preview_image(Image::from_rgba8(buffer));
}

fn preview_state_text(
    application: &Application,
    preview: &pw_graph_backend::video::VideoPreviewHandle,
) -> String {
    use pw_graph_video::preview::PreviewState;

    match preview.state() {
        PreviewState::Idle => application.t("video.preview_idle"),
        PreviewState::Live => {
            if let Some(spec) = preview.spec() {
                format!(
                    "{}x{}@{:.0} {}",
                    spec.width,
                    spec.height,
                    spec.fps_f64(),
                    spec.format.as_str()
                )
            } else {
                application.t("video.preview_live")
            }
        }
        PreviewState::Ended => application.t("video.preview_ended"),
        PreviewState::Error(message) => {
            application.tf("video.preview_error", &[("error", message)])
        }
    }
}

/// Downscale RGBA bytes to at most 480p for dialog display. Nearest-neighbor
/// keeps it dependency-free and fast enough for a UI tick.
fn downscale_rgba_for_preview(rgba: &[u8], width: u32, height: u32) -> (Vec<Rgba8Pixel>, u32, u32) {
    const MAX_DIMENSION: u32 = 640;
    let scale = width.max(height).div_ceil(MAX_DIMENSION).max(1);
    if scale == 1 {
        return (
            rgba.chunks_exact(4)
                .map(|px| Rgba8Pixel {
                    r: px[0],
                    g: px[1],
                    b: px[2],
                    a: px[3],
                })
                .collect(),
            width,
            height,
        );
    }
    let (out_w, out_h) = ((width / scale).max(1), (height / scale).max(1));
    let mut pixels = Vec::with_capacity((out_w * out_h) as usize);
    for out_y in 0..out_h {
        for out_x in 0..out_w {
            let src_x = (out_x * scale).min(width - 1) as usize;
            let src_y = (out_y * scale).min(height - 1) as usize;
            let offset = (src_y * width as usize + src_x) * 4;
            pixels.push(Rgba8Pixel {
                r: rgba[offset],
                g: rgba[offset + 1],
                b: rgba[offset + 2],
                a: rgba[offset + 3],
            });
        }
    }
    (pixels, out_w, out_h)
}

/// Summaries for every node the backend tracks video state for, keyed by
/// node id for card subtitles.
pub(crate) fn video_summaries_by_node(
    application: &Application,
) -> std::collections::BTreeMap<NodeId, String> {
    use std::collections::BTreeSet;

    let filter_nodes: BTreeSet<NodeId> = application
        .source
        .video_filters()
        .iter()
        .map(|filter| filter.node_id)
        .collect();
    // Filter nodes plus any external video node the backend tracks state
    // for (the screen-cast stream).
    let mut node_ids: BTreeSet<NodeId> = filter_nodes;
    node_ids.extend(application.source.graph().nodes.keys().copied());
    node_ids
        .into_iter()
        .filter_map(|node_id| {
            application
                .source
                .video_node_info(node_id)
                .map(|info| (node_id, video_node_summary(&info)))
        })
        .collect()
}

/// One-line video summary for node cards: resolution, fps, format, state,
/// and dropped frames.
pub(crate) fn video_node_summary(info: &VideoNodeInfo) -> String {
    let mut parts = Vec::new();
    if let Some(spec) = &info.spec {
        parts.push(format!(
            "{}x{}@{:.0} {}",
            spec.width,
            spec.height,
            spec.fps_f64(),
            spec.format.as_str()
        ));
    }
    parts.push(info.state.as_str().to_owned());
    if let Some(counters) = &info.counters {
        if counters.frames_dropped > 0 {
            parts.push(format!("dropped {}", counters.frames_dropped));
        }
    }
    parts.join(" · ")
}

#[cfg(test)]
mod tests {
    use super::super::tests::demo_application;
    use super::*;

    #[test]
    fn add_and_remove_filter_through_handlers() {
        let mut application = demo_application();
        add_video_filter(&mut application, "grayscale");
        assert_eq!(application.source.video_filters().len(), 1);
        assert!(application.status.contains("grayscale"));

        // Geometry filters need parameters.
        add_video_filter(&mut application, "crop");
        assert_eq!(application.source.video_filters().len(), 1);
        add_video_filter(&mut application, "crop;crop=0,0,64,64");
        assert_eq!(application.source.video_filters().len(), 2);
        add_video_filter(&mut application, "scale;scale=32x32");
        assert_eq!(application.source.video_filters().len(), 3);
        add_video_filter(&mut application, "bloom");
        assert_eq!(application.source.video_filters().len(), 3);

        // Removal works on the selected filter node.
        let node = application.source.video_filters()[0].node_id;
        application.view.selected_nodes.clear();
        application.view.selected_nodes.insert(node);
        remove_selected_video_filter(&mut application);
        assert_eq!(application.source.video_filters().len(), 2);

        // Nothing selected: a status hint, not a panic.
        application.view.selected_nodes.clear();
        remove_selected_video_filter(&mut application);
        assert_eq!(application.source.video_filters().len(), 2);
    }

    #[test]
    fn preview_opens_polls_and_closes() {
        let window = super::super::tests::test_window();
        let mut application = demo_application();
        add_video_filter(&mut application, "passthrough");
        let instance = application.source.video_filters()[0].instance_id.clone();

        open_preview(&mut application, Some(&instance));
        poll_video_preview(&window, &mut application);
        assert!(window.get_show_video_preview());
        assert!(window.get_video_preview_title().contains(&instance));

        close_preview(&window, &mut application);
        assert!(!window.get_show_video_preview());
        poll_video_preview(&window, &mut application);
        assert!(!window.get_show_video_preview());
    }

    #[test]
    fn summaries_show_format_state_and_drops() {
        use pw_graph_backend::video::VideoNodeState;

        let spec = pw_graph_video::VideoSpec::new(
            1280,
            720,
            60,
            1,
            pw_graph_video::VideoPixelFormat::Nv12,
        )
        .unwrap();
        let mut counters = pw_graph_video::VideoDiagnostics::new().snapshot();
        counters.frames_dropped = 7;
        let info = VideoNodeInfo {
            node_id: NodeId(1),
            instance_id: Some("a".into()),
            spec: Some(spec),
            state: pw_graph_backend::video::VideoNodeState::Live,
            counters: Some(counters),
            preview_state: pw_graph_video::preview::PreviewState::Live,
        };
        let summary = video_node_summary(&info);
        assert!(summary.contains("1280x720@60 NV12"), "{summary}");
        assert!(summary.contains(VideoNodeState::Live.as_str()), "{summary}");
        assert!(summary.contains("dropped 7"), "{summary}");
    }

    #[test]
    fn virtual_geometry_parsing_falls_back_safely() {
        assert_eq!(
            parse_virtual_geometry(None),
            VirtualDisplayRequest::default()
        );
        let parsed = parse_virtual_geometry(Some("2560x1440@60"));
        assert_eq!(
            (parsed.width, parsed.height, parsed.refresh_hz),
            (2560, 1440, 60)
        );
        assert_eq!(
            parse_virtual_geometry(Some("nonsense")),
            VirtualDisplayRequest::default()
        );
        assert_eq!(
            parse_virtual_geometry(Some("0x0@0")),
            VirtualDisplayRequest::default()
        );
    }
}
