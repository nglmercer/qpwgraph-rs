//! Linux video UI actions: capture, virtual displays, filters, previews.
//!
//! Handlers run on the Slint event thread. Starting a portal capture blocks
//! that thread until the compositor dialog resolves — the dialog is modal and
//! immediate, and drivers are `!Send`, so an async ticket would add machinery
//! without changing what the user sees. Everything else here is non-blocking:
//! previews poll the newest frame on the 50 ms pump and never touch the
//! realtime thread.

use pw_graph_backend::video::{
    ScreenCastRequest, ScreenCastSource, ScreenCastState, VideoFilterInstance, VideoFilterRequest,
    VideoNodeInfo, VideoNodeState, VirtualDisplayRequest,
};
use pw_graph_core::NodeId;
use slint::{Image, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, SharedString, VecModel};
use std::rc::Rc;

use super::app::Application;
use super::{EffectRow, MainWindow};
use crate::model::VideoPanel;
use crate::source::ApplicationDriver;
use pw_graph_i18n::I18n;
use std::collections::BTreeMap;

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
    pub(crate) fn next_instance_id(&mut self, filter_id: &str) -> String {
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
/// `;scale=WxH` parameters for the geometry filters. Returns the instance
/// on success so the effects dialog can apply the draft's enabled flag.
pub(crate) fn add_video_filter(
    application: &mut Application,
    spec: &str,
) -> Option<VideoFilterInstance> {
    if !require_video(application) {
        return None;
    }
    if !application.source.capabilities().connect {
        application.status = application.t("status.connections_unavailable");
        return None;
    }
    let (filter_id, params) = match parse_filter_spec(spec) {
        Ok(parsed) => parsed,
        Err(message) => {
            application.status =
                application.tf("status.video_filter_failed", &[("error", message)]);
            return None;
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
            Some(instance)
        }
        Err(error) => {
            application.status = application.tf("status.video_filter_failed", &[("error", error)]);
            None
        }
    }
}

pub(crate) fn parse_filter_spec(
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

/// Remove one filter by instance id. Used by the effects-dialog video tab.
pub(crate) fn remove_video_filter_by_id(application: &mut Application, instance_id: &str) {
    if !require_video(application) {
        return;
    }
    match application.source.remove_video_filter(instance_id) {
        Ok(()) => {
            if application.video.preview == PreviewTarget::Filter(instance_id.to_owned()) {
                application.video.preview = PreviewTarget::None;
            }
            application.status = application.tf(
                "status.video_filter_removed",
                &[("name", instance_id.to_owned())],
            );
            let _ = application.source.refresh();
        }
        Err(error) => {
            application.status = application.tf("status.video_filter_failed", &[("error", error)]);
        }
    }
}

/// Flip one filter between enabled and bypassed by instance id. Used by
/// the effects-dialog video tab.
pub(crate) fn toggle_video_filter(application: &mut Application, instance_id: &str) {
    if !require_video(application) {
        return;
    }
    let Some(node_id) = application
        .source
        .video_filters()
        .iter()
        .find(|filter| filter.instance_id == instance_id)
        .map(|filter| filter.node_id)
    else {
        application.status = application.t("status.video_select_filter");
        return;
    };
    // Enable state is not tracked locally; enabling an already-enabled
    // filter is a no-op, so toggle via disable-then-decide is avoidable:
    // query node info and flip Bypassed <-> Live/Idle.
    let disable = application
        .source
        .video_node_info(node_id)
        .is_none_or(|info| info.state != VideoNodeState::Bypassed);
    match application
        .source
        .set_video_filter_enabled(instance_id, !disable)
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

/// Effects-dialog video tab: the same gallery workflow as audio, backed by
/// video filter instances. Dialog components, draft state, and verbs are
/// shared; only this data source differs.
#[derive(Clone, Copy, Debug)]
pub(crate) struct VideoCatalogEntry {
    /// Backend filter id, also the draft/selection identity.
    pub(crate) id: &'static str,
    /// Short localized name key (`video.name_*`).
    pub(crate) name_key: &'static str,
    /// Creation spec; geometry filters carry the same defaults as the rail.
    pub(crate) spec: &'static str,
}

pub(crate) const VIDEO_FILTER_CATALOG: &[VideoCatalogEntry] = &[
    VideoCatalogEntry {
        id: "passthrough",
        name_key: "video.name_passthrough",
        spec: "passthrough",
    },
    VideoCatalogEntry {
        id: "grayscale",
        name_key: "video.name_grayscale",
        spec: "grayscale",
    },
    VideoCatalogEntry {
        id: "hflip",
        name_key: "video.name_hflip",
        spec: "hflip",
    },
    VideoCatalogEntry {
        id: "vflip",
        name_key: "video.name_vflip",
        spec: "vflip",
    },
    VideoCatalogEntry {
        id: "crop",
        name_key: "video.name_crop",
        spec: "crop;crop=0,0,640,480",
    },
    VideoCatalogEntry {
        id: "scale",
        name_key: "video.name_scale",
        spec: "scale;scale=640x480",
    },
];

pub(crate) fn video_catalog_entry(filter_id: &str) -> Option<&'static VideoCatalogEntry> {
    VIDEO_FILTER_CATALOG
        .iter()
        .find(|entry| entry.id == filter_id)
}

/// ComboBox options for the video tab, in catalog order.
pub(crate) fn video_effect_options(i18n: &I18n) -> Vec<SharedString> {
    VIDEO_FILTER_CATALOG
        .iter()
        .map(|entry| SharedString::from(i18n.text(entry.name_key)))
        .collect()
}

/// Map video states onto the dialog's health tokens so rows keep the same
/// color coding: red failed, purple bypassed, teal live, gray idle.
fn video_health_label(state: VideoNodeState) -> &'static str {
    match state {
        VideoNodeState::Live => "HEALTHY",
        VideoNodeState::Bypassed => "BYPASSED",
        VideoNodeState::Failed => "FAILED",
        VideoNodeState::Idle => "IDLE",
    }
}

/// Gallery rows for the video tab, reusing the audio `EffectRow` shape.
/// Video has no runtime parameter API, so rows carry no sliders; the
/// subtitle shows the same summary line as the canvas cards.
pub(crate) fn video_effect_rows(source: &ApplicationDriver, i18n: &I18n) -> Vec<EffectRow> {
    let mut filters = source.video_filters();
    filters.sort_by(|a, b| a.instance_id.cmp(&b.instance_id));
    filters
        .into_iter()
        .map(|filter| {
            let info = source.video_node_info(filter.node_id);
            let name = video_catalog_entry(&filter.filter_id)
                .map(|entry| i18n.text(entry.name_key))
                .unwrap_or_else(|| filter.filter_id.clone());
            let vendor = info
                .as_ref()
                .map(video_node_summary)
                .unwrap_or_else(|| filter.filter_id.clone());
            let state = info.as_ref().map(|info| info.state);
            EffectRow {
                instance_id: SharedString::from(filter.instance_id.clone()),
                name: SharedString::from(name),
                vendor: SharedString::from(vendor),
                health: SharedString::from(state.map(video_health_label).unwrap_or("IDLE")),
                diagnostics: SharedString::new(),
                description: SharedString::from(filter.instance_id),
                enabled: state.is_none_or(|state| state != VideoNodeState::Bypassed),
                parameters: ModelRc::from(Rc::new(VecModel::default())),
            }
        })
        .collect()
}

fn video_row_vitals_equal(current: &EffectRow, next: &EffectRow) -> bool {
    // `parameters` is always an empty model for video; compare everything
    // else so an unchanged gallery survives the sync untouched.
    current.instance_id == next.instance_id
        && current.name == next.name
        && current.vendor == next.vendor
        && current.health == next.health
        && current.diagnostics == next.diagnostics
        && current.description == next.description
        && current.enabled == next.enabled
}

/// Synchronize video rows like the audio gallery: rebuild the model when
/// membership changes, otherwise patch changed rows in place.
pub(crate) fn sync_video_effect_rows(
    current: ModelRc<EffectRow>,
    source: &ApplicationDriver,
    i18n: &I18n,
) -> Option<ModelRc<EffectRow>> {
    let rows = video_effect_rows(source, i18n);
    let Some(model) = current.as_any().downcast_ref::<VecModel<EffectRow>>() else {
        return Some(ModelRc::from(Rc::new(VecModel::from(rows))));
    };
    let same_members = model.row_count() == rows.len()
        && rows.iter().enumerate().all(|(index, row)| {
            model
                .row_data(index)
                .is_some_and(|current| current.instance_id == row.instance_id)
        });
    if !same_members {
        return Some(ModelRc::from(Rc::new(VecModel::from(rows))));
    }
    for (index, row) in rows.into_iter().enumerate() {
        if model
            .row_data(index)
            .is_some_and(|current| video_row_vitals_equal(&current, &row))
        {
            continue;
        }
        model.set_row_data(index, row);
    }
    None
}

pub(crate) fn select_video_draft(window: &MainWindow, application: &mut Application, index: usize) {
    if let Some(entry) = VIDEO_FILTER_CATALOG.get(index) {
        application.effect_selection_id = Some(entry.id.to_owned());
    }
    window.set_effect_selection_index(index as i32);
    prepare_video_draft(window, application);
}

pub(crate) fn prepare_video_draft(window: &MainWindow, application: &mut Application) {
    let entry = application
        .effect_selection_id
        .as_deref()
        .and_then(video_catalog_entry)
        .or(VIDEO_FILTER_CATALOG.first());
    let Some(entry) = entry else {
        application.effect_draft_id = None;
        application.effect_draft_parameters.clear();
        window.set_effect_configuring(false);
        application.status = application.t("status.video_unavailable");
        return;
    };
    let index = VIDEO_FILTER_CATALOG
        .iter()
        .position(|candidate| candidate.id == entry.id)
        .unwrap_or(0);
    application.effect_draft_id = Some(entry.id.to_owned());
    application.effect_selection_id = Some(entry.id.to_owned());
    application.effect_draft_enabled = true;
    application.effect_draft_parameters.clear();
    window.set_effect_selection_index(index as i32);
    window.set_effect_configuring(true);
}

/// Video half of the shared Add/create flow: first call opens the setup
/// form, the second creates the filter. Creation is synchronous (no
/// tickets), then the draft closes like the audio one.
pub(crate) fn create_video_effect(window: &MainWindow, application: &mut Application) {
    let entry = application
        .effect_selection_id
        .as_deref()
        .and_then(video_catalog_entry)
        .or(VIDEO_FILTER_CATALOG.first());
    let Some(entry) = entry else {
        application.status = application.t("status.video_unavailable");
        return;
    };
    if application.effect_draft_id.as_deref() != Some(entry.id) || !window.get_effect_configuring()
    {
        prepare_video_draft(window, application);
        application.status = application.t("effects.setup_hint");
        return;
    }
    let enabled = application.effect_draft_enabled;
    if let Some(instance) = add_video_filter(application, entry.spec) {
        if !enabled {
            let _ = application
                .source
                .set_video_filter_enabled(&instance.instance_id, false);
        }
        super::effects::finish_effect_setup(window, application);
    }
}

/// Inspect opens a live preview of the instance: the video equivalent of
/// the audio setup sheet.
pub(crate) fn inspect_video_filter(application: &mut Application, instance_id: &str) {
    if !require_video(application) {
        return;
    }
    if !application
        .source
        .video_filters()
        .iter()
        .any(|filter| filter.instance_id == instance_id)
    {
        application.status = application.t("status.video_select_filter");
        return;
    }
    open_preview(application, Some(instance_id));
    application.status = application.tf(
        "video.preview_filter_title",
        &[("name", instance_id.to_owned())],
    );
}

/// Fill the shared effect-diagnostics dialog from the instance's node info.
pub(crate) fn debug_video_filter(
    window: &MainWindow,
    application: &mut Application,
    instance_id: &str,
) {
    if !require_video(application) {
        return;
    }
    let instance = application
        .source
        .video_filters()
        .into_iter()
        .find(|filter| filter.instance_id == instance_id);
    let Some(instance) = instance else {
        application.status = application.t("status.video_select_filter");
        return;
    };
    let info = application.source.video_node_info(instance.node_id);
    let name = video_catalog_entry(&instance.filter_id)
        .map(|entry| application.t(entry.name_key))
        .unwrap_or_else(|| instance.filter_id.clone());
    let mut report = format!(
        "filter: {} ({})\nnode: {}\n",
        name, instance.instance_id, instance.node_id.0
    );
    match info {
        Some(info) => {
            report.push_str(&format!("state: {}\n", info.state.as_str()));
            match &info.spec {
                Some(spec) => report.push_str(&format!("spec: {spec}\n")),
                None => report.push_str("spec: -\n"),
            }
            if let Some(counters) = &info.counters {
                report.push_str(&format!(
                    "received {} · processed {} · output {} · dropped {} · bypassed {} · rejected {}\nqueue: {}/{}\nlast process: {}us · max: {}us\n",
                    counters.frames_received,
                    counters.frames_processed,
                    counters.frames_output,
                    counters.frames_dropped,
                    counters.frames_bypassed,
                    counters.frames_rejected,
                    counters.queue_depth,
                    counters.queue_capacity,
                    counters.last_processing_time_us,
                    counters.max_processing_time_us,
                ));
            }
            report.push_str(&format!("preview: {}\n", preview_word(&info.preview_state)));
            application.effect_debug_health = video_health_label(info.state).to_owned();
        }
        None => {
            report.push_str("state: unknown (no node info)\n");
            application.effect_debug_health = "IDLE".to_owned();
        }
    }
    application.effect_debug_name = name;
    application.effect_debug_report = report;
    window.set_show_effect_diagnostics(true);
}

fn preview_word(state: &pw_graph_video::preview::PreviewState) -> &'static str {
    match state {
        pw_graph_video::preview::PreviewState::Idle => "idle",
        pw_graph_video::preview::PreviewState::Live => "live",
        pw_graph_video::preview::PreviewState::Ended => "ended",
        pw_graph_video::preview::PreviewState::Error(_) => "error",
    }
}

/// Node cards that show the video action block: filter instances get
/// preview, the active capture gets preview plus stop.
pub(crate) fn video_panels_by_node(application: &Application) -> BTreeMap<NodeId, VideoPanel> {
    let mut panels = BTreeMap::new();
    if !video_available(application) {
        return panels;
    }
    for filter in application.source.video_filters() {
        panels.insert(
            filter.node_id,
            VideoPanel {
                preview: true,
                stop: false,
            },
        );
    }
    if let Some(node_id) = application.source.screen_cast_node() {
        panels.insert(
            node_id,
            VideoPanel {
                preview: true,
                stop: true,
            },
        );
    }
    panels
}

/// Card Preview button: preview a filter instance or the active capture.
pub(crate) fn preview_node_video(application: &mut Application, rendered_id: i32) {
    if !require_video(application) {
        return;
    }
    let Some(node_id) = application.view.ids.node_id(rendered_id) else {
        return;
    };
    if let Some(instance_id) = application
        .source
        .video_filters()
        .into_iter()
        .find(|filter| filter.node_id == node_id)
        .map(|filter| filter.instance_id)
    {
        open_preview(application, Some(&instance_id));
    } else if application.source.screen_cast_node() == Some(node_id) {
        open_preview(application, Some("capture"));
    }
}

/// Card Stop button: only the active capture offers it.
pub(crate) fn stop_node_video(application: &mut Application, rendered_id: i32) {
    if !require_video(application) {
        return;
    }
    let Some(node_id) = application.view.ids.node_id(rendered_id) else {
        return;
    };
    if application.source.screen_cast_node() == Some(node_id) {
        stop_capture(application);
    }
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

        // Removal works by instance id.
        let instance = application.source.video_filters()[0].instance_id.clone();
        remove_video_filter_by_id(&mut application, &instance);
        assert_eq!(application.source.video_filters().len(), 2);

        // Unknown id: a backend error, not a panic.
        remove_video_filter_by_id(&mut application, "no-such-filter");
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

    #[test]
    fn video_tab_lists_the_catalog_and_starts_empty() {
        use super::super::app::EffectMediaTab;

        let application = demo_application();
        assert_eq!(application.effect_media_tab, EffectMediaTab::Audio);
        let options = video_effect_options(&application.i18n);
        let names: Vec<String> = options.iter().map(|name| name.to_string()).collect();
        assert_eq!(
            names,
            vec![
                "Passthrough",
                "Grayscale",
                "Horizontal flip",
                "Vertical flip",
                "Crop (0,0 640x480)",
                "Scale (640x480)"
            ]
        );
        assert!(video_effect_rows(&application.source, &application.i18n).is_empty());
    }

    #[test]
    fn video_dialog_runs_the_shared_gallery_workflow() {
        use super::super::app::EffectMediaTab;
        use super::super::effects;

        let window = super::super::tests::test_window();
        let mut application = demo_application();
        effects::select_effect_media_tab(&window, &mut application, 1);
        assert_eq!(application.effect_media_tab, EffectMediaTab::Video);
        assert_eq!(window.get_effect_media_tab(), 1);

        // Pick a catalog entry: opens the setup form like audio does.
        effects::select_effect_draft(&window, &mut application, 1);
        assert_eq!(application.effect_draft_id.as_deref(), Some("grayscale"));
        assert!(window.get_effect_configuring());

        // Confirm: creates the filter and closes the form.
        effects::create_effect(&window, &mut application);
        assert!(!window.get_effect_configuring());
        assert_eq!(application.source.video_filters().len(), 1);
        let rows = video_effect_rows(&application.source, &application.i18n);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name.as_str(), "Grayscale");
        assert_eq!(rows[0].health.as_str(), "IDLE");
        assert!(rows[0].enabled);
        assert!(rows[0].vendor.as_str().contains("idle"));
        let instance_id = rows[0].instance_id.to_string();

        // Row verbs route to the video backend through the shared dispatch.
        effects::toggle_effect(&mut application, &instance_id);
        let rows = video_effect_rows(&application.source, &application.i18n);
        assert_eq!(rows[0].health.as_str(), "BYPASSED");
        assert!(!rows[0].enabled);
        effects::toggle_effect(&mut application, &instance_id);
        let rows = video_effect_rows(&application.source, &application.i18n);
        assert!(rows[0].enabled);

        effects::inspect_effect(&mut application, Some(&instance_id));
        assert_eq!(
            application.video.preview,
            PreviewTarget::Filter(instance_id.clone())
        );

        effects::open_effect_diagnostics(&window, &mut application, Some(&instance_id));
        assert!(window.get_show_effect_diagnostics());
        assert!(application.effect_debug_report.contains("grayscale"));

        effects::remove_effect(&mut application, &instance_id);
        assert!(application.source.video_filters().is_empty());
        assert_eq!(application.video.preview, PreviewTarget::None);

        // Back to audio: the tab and the window property follow together.
        effects::select_effect_media_tab(&window, &mut application, 0);
        assert_eq!(application.effect_media_tab, EffectMediaTab::Audio);
        assert_eq!(window.get_effect_media_tab(), 0);
    }

    #[test]
    fn switching_tabs_discards_the_unsubmitted_draft() {
        use super::super::app::EffectMediaTab;
        use super::super::effects;

        let window = super::super::tests::test_window();
        let mut application = demo_application();
        effects::prepare_effect_draft(&window, &mut application);
        assert!(application.effect_draft_id.is_some());

        effects::select_effect_media_tab(&window, &mut application, 1);
        assert_eq!(application.effect_media_tab, EffectMediaTab::Video);
        assert!(application.effect_draft_id.is_none());
        assert!(application.effect_selection_id.is_none());
        assert!(!window.get_effect_configuring());

        effects::select_effect_media_tab(&window, &mut application, 7);
        assert_eq!(application.effect_media_tab, EffectMediaTab::Audio);
        assert_eq!(window.get_effect_media_tab(), 0);
    }

    #[test]
    fn video_row_sync_rebuilds_on_membership_and_patches_vitals() {
        use slint::Model;

        let mut application = demo_application();
        let current = ModelRc::from(Rc::new(VecModel::from(video_effect_rows(
            &application.source,
            &application.i18n,
        ))));
        assert!(
            sync_video_effect_rows(current.clone(), &application.source, &application.i18n)
                .is_none()
        );

        add_video_filter(&mut application, "grayscale");
        let rebuilt =
            sync_video_effect_rows(current, &application.source, &application.i18n).unwrap();
        assert_eq!(
            rebuilt
                .as_any()
                .downcast_ref::<VecModel<EffectRow>>()
                .unwrap()
                .row_count(),
            1
        );

        // Same membership: state flips patch the row in place.
        let instance_id = application.source.video_filters()[0].instance_id.clone();
        toggle_video_filter(&mut application, &instance_id);
        assert!(
            sync_video_effect_rows(rebuilt.clone(), &application.source, &application.i18n)
                .is_none()
        );
        let row = rebuilt
            .as_any()
            .downcast_ref::<VecModel<EffectRow>>()
            .unwrap()
            .row_data(0)
            .unwrap();
        assert_eq!(row.health.as_str(), "BYPASSED");
        assert!(!row.enabled);
    }

    #[test]
    fn panels_cover_filters_with_preview_and_capture_with_stop() {
        use pw_graph_backend::video::{ScreenCastRequest, ScreenCastSource};

        let mut application = demo_application();
        assert!(video_panels_by_node(&application).is_empty());

        add_video_filter(&mut application, "grayscale");
        let filter_node = application.source.video_filters()[0].node_id;
        let panels = video_panels_by_node(&application);
        assert_eq!(
            panels.get(&filter_node),
            Some(&VideoPanel {
                preview: true,
                stop: false,
            })
        );

        application
            .source
            .start_screen_cast(ScreenCastRequest {
                source: ScreenCastSource::Monitor,
                show_cursor: true,
                multiple: false,
            })
            .unwrap();
        let capture_node = application.source.screen_cast_node().unwrap();
        let panels = video_panels_by_node(&application);
        assert_eq!(
            panels.get(&capture_node),
            Some(&VideoPanel {
                preview: true,
                stop: true,
            })
        );
        // The filter keeps its preview-only panel while capturing.
        assert_eq!(
            panels.get(&filter_node),
            Some(&VideoPanel {
                preview: true,
                stop: false,
            })
        );
    }

    #[test]
    fn card_actions_preview_and_stop_by_rendered_id() {
        use pw_graph_backend::video::{ScreenCastRequest, ScreenCastSource};

        let mut application = demo_application();
        add_video_filter(&mut application, "grayscale");
        let filter_node = application.source.video_filters()[0].node_id;
        application
            .source
            .start_screen_cast(ScreenCastRequest {
                source: ScreenCastSource::Monitor,
                show_cursor: true,
                multiple: false,
            })
            .unwrap();
        let capture_node = application.source.screen_cast_node().unwrap();
        // Rendered ids resolve through the view map like the card callbacks.
        application.view.ids.rebuild(application.source.graph());
        let filter_card = application.view.ids.node(filter_node).unwrap();
        let capture_card = application.view.ids.node(capture_node).unwrap();

        // Filter card previews its instance.
        preview_node_video(&mut application, filter_card);
        assert_eq!(
            application.video.preview,
            PreviewTarget::Filter(application.source.video_filters()[0].instance_id.clone())
        );

        // Capture card previews the stream, then stops the session.
        preview_node_video(&mut application, capture_card);
        assert_eq!(application.video.preview, PreviewTarget::Capture);
        stop_node_video(&mut application, capture_card);
        assert!(application.source.screen_cast_node().is_none());
        assert_eq!(application.video.preview, PreviewTarget::None);

        // Unknown cards and non-video cards are ignored, not fatal.
        stop_node_video(&mut application, filter_card);
        preview_node_video(&mut application, 424242);
        stop_node_video(&mut application, 424242);
    }
}
