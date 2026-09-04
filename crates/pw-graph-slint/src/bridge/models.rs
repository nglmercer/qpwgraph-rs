use crate::canvas::{self, CanvasGeometry, LinkGeometry, NodeGeometry, PinGeometry};
use crate::model::{
    node_type_color, ConnectMode, GraphSnapshot, LinkView, MeterState, NodeBackendProfile, NodeView,
};
use crate::source::ApplicationDriver;
use pw_graph_config::{config_path, AppConfig};
use pw_graph_core::{Direction, NodeId};
use pw_graph_i18n::I18n;
use pw_graph_patchbay::Patchbay;
#[cfg(not(feature = "relay"))]
use slint::Image;
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::Rc;

use super::app::{toast_visible, Application};
use super::effects::{effect_options, effect_rows, effect_setup_rows};
use super::meters::meter_fallback;
#[cfg(feature = "relay")]
use super::relay::relay_qr_payload;
#[cfg(feature = "relay")]
use super::relay::{qr_image, relay_host_endpoint};
use super::relay::{
    relay_codec_index, relay_frame_index, relay_mode_tab, relay_nodes_visible, relay_rows,
    relay_transport_index,
};
use super::utils::{
    color, language_index, localized_meter_label, localized_node_type, meter_fraction,
    meter_policy_index, track_position_from_volume,
};
use super::{
    HistoryRow, LinkRow, MainWindow, MinimapNode, NodeRow, PortRow, RuleRow, ShortcutRow, UiI18n,
};
use crate::names::{compact_label, display_node_name, display_port_name};
pub(crate) fn sync_models(
    window: &MainWindow,
    application: &mut Application,
    nodes: &Rc<VecModel<NodeRow>>,
    links: &Rc<VecModel<LinkRow>>,
    minimap_nodes: &Rc<VecModel<MinimapNode>>,
    geometry: &Rc<RefCell<CanvasGeometry>>,
    geometry_version: &Rc<Cell<i32>>,
) {
    application.view.relay_nodes_visible = relay_nodes_visible(application);
    let backend_profiles = read_backend_profiles(&application.source);
    let snapshot = application.view.snapshot_with_meters(
        application.source.graph(),
        &application.config,
        &application.meters,
        meter_fallback(&application.source),
        &backend_profiles,
    );
    let node_rows = snapshot
        .nodes
        .iter()
        .map(|node| node_row(node, &application.i18n))
        .collect::<Vec<_>>();
    let rows_applied = sync_node_rows(window, nodes, node_rows);
    links.set_vec(snapshot.links.iter().map(link_row).collect::<Vec<_>>());
    minimap_nodes.set_vec(
        snapshot
            .nodes
            .iter()
            .map(|node| MinimapNode {
                id: node.id,
                x: node.position[0],
                y: node.position[1],
                width: node.width,
                height: node.height,
                color: color(
                    node.appearance
                        .color
                        .unwrap_or_else(|| node_type_color(node.node_type)),
                ),
            })
            .collect::<Vec<_>>(),
    );
    // Only refresh the cache when the rendered rows were refreshed with it,
    // so a gesture in flight can never hit-test against geometry the user
    // cannot see.
    if rows_applied || geometry.borrow().is_empty() {
        rebuild_geometry(geometry, &snapshot, application.view.connect_mode);
        geometry_version.set(geometry_version.get().wrapping_add(1));
        window.set_geometry_version(geometry_version.get());
        let bounds = graph_bounds(&snapshot);
        window.set_graph_min_x(bounds[0]);
        window.set_graph_min_y(bounds[1]);
        window.set_graph_max_x(bounds[2]);
        window.set_graph_max_y(bounds[3]);
    }
    let (node_count, port_count, link_count) = application.view.visible_counts(&snapshot);
    window.set_status(SharedString::from(application.status.clone()));
    window.set_toast_message(SharedString::from(application.toast_message.clone()));
    window.set_toast_visible(toast_visible(application));
    window.set_toast_error(application.toast_error);
    let backend = if application.debug {
        application.i18n.format(
            "debug.backend",
            &[
                ("backend", application.source.backend_name().to_owned()),
                ("enabled", application.source.has_alsa().to_string()),
            ],
        )
    } else {
        String::new()
    };
    window.set_backend(SharedString::from(backend));
    window.set_has_selected_link(!application.view.selected_links.is_empty());
    window.set_has_selected_node(!application.view.selected_nodes.is_empty());
    window.set_connections_available(application.source.capabilities().connect);
    window.set_graph_counts(SharedString::from(application.i18n.format(
        "status.graph_counts",
        &[
            ("nodes", node_count.to_string()),
            ("ports", port_count.to_string()),
            ("links", link_count.to_string()),
        ],
    )));
    window.set_show_minimap(application.view.minimap_visible);
    window.set_sort_type(SharedString::from(if application.view.sort_ports_by_name {
        "name"
    } else {
        "id"
    }));
    window.set_sort_order(SharedString::from(
        if application.view.sort_ports_descending {
            "descending"
        } else {
            "ascending"
        },
    ));
    window.set_media_filter(SharedString::from(application.view.media_filter.as_str()));
    window.set_connect_mode(SharedString::from(application.view.connect_mode.as_str()));
    window.set_thumbnail_view(application.view.thumbnail_mode);
    window.set_show_common_actions(application.config.toolbar);
    window.set_show_patchbay_toolbar(application.config.patchbay_toolbar);
    window.set_repel_overlaps(application.config.repel_overlapping_nodes);
    window.set_connect_through(application.config.connect_through_nodes);
    window.set_language_index(language_index(&application.config.language));
    window
        .global::<UiI18n>()
        .set_version(language_index(&application.config.language));
    window.set_meter_policy_index(meter_policy_index(application.source.meter_policy()));
    window.set_ui_text_scale(application.config.ui_text_scale);
    window.set_panel_text_scale(application.config.panel_text_scale);
    window.set_node_text_scale(application.view.node_text_scale);
    window
        .global::<super::UiTheme>()
        .set_ui_scale(application.config.ui_text_scale);
    window
        .global::<super::UiTheme>()
        .set_panel_scale(application.config.panel_text_scale);
    window
        .global::<super::UiTheme>()
        .set_node_scale(application.view.node_text_scale);
    window.set_patchbay_exclusive(application.config.patchbay_exclusive);
    window.set_patchbay_auto_disconnect(application.config.patchbay_auto_disconnect);
    window.set_patchbay_auto_pin(application.config.patchbay_auto_pin);
    window.set_patchbay_activated(application.config.patchbay_activated);
    window.set_profile_name(SharedString::from(
        application.config.active_patchbay_profile.clone(),
    ));
    window.set_profile_options(string_model(profile_options(&application.config)));
    window.set_profile_index(profile_index(&application.config));
    window.set_recent_patchbay_paths(string_model(recent_patchbay_paths(&application.config)));
    window.set_zoom(application.view.zoom);
    window.set_pan_x(application.view.pan[0]);
    window.set_pan_y(application.view.pan[1]);
    window.set_relay_device_name(SharedString::from(
        application.config.relay_device_name.clone(),
    ));
    window.set_relay_host_pin(SharedString::from(
        application.config.relay_host_pin.clone(),
    ));
    window.set_relay_host_port_text(SharedString::from(
        application.config.relay_host_port.to_string(),
    ));
    window.set_relay_client_target(SharedString::from(
        application.config.relay_client_target.clone(),
    ));
    window.set_relay_client_pin(SharedString::from(
        application.config.relay_client_pin.clone(),
    ));
    window.set_relay_auto_connect_trusted(application.config.relay_auto_connect_trusted);
    if window.get_relay_tab() < 2 {
        window.set_relay_tab(relay_mode_tab(application.config.relay_mode));
    }
    window.set_relay_codec_index(relay_codec_index(&application.config.relay_codec));
    window.set_relay_frame_index(relay_frame_index(application.config.relay_frame_ms));
    window.set_relay_transport_index(relay_transport_index(&application.config.relay_transport));
    window.set_relay_codec_options(string_model([
        application.i18n.text("relay.codec_opus"),
        application.i18n.text("relay.codec_pcm"),
    ]));
    window.set_relay_frame_options(string_model(
        [5, 10, 20, 40, 60]
            .into_iter()
            .map(|frame| {
                application
                    .i18n
                    .format("relay.frame_option", &[("frame", frame.to_string())])
            })
            .collect::<Vec<_>>(),
    ));
    window.set_relay_transport_options(string_model([
        application.i18n.text("relay.transport_auto"),
        application.i18n.text("relay.transport_wifi"),
        application.i18n.text("relay.transport_bluetooth_pan"),
        application.i18n.text("relay.transport_lan"),
        application.i18n.text("relay.transport_adb"),
    ]));
    #[cfg(feature = "relay")]
    {
        let send_options = relay_send_source_options(application);
        let receive_options = relay_receive_sink_options(application);
        window.set_relay_send_source_options(string_model(
            send_options.iter().map(|(_, name)| name.clone()),
        ));
        window.set_relay_receive_sink_options(string_model(
            receive_options.iter().map(|(_, name)| name.clone()),
        ));
        window.set_relay_send_source_index(relay_selector_index(
            &application.config.relay_send_source,
            &send_options,
        ));
        window.set_relay_receive_sink_index(relay_selector_index(
            &application.config.relay_receive_sink,
            &receive_options,
        ));
    }
    #[cfg(not(feature = "relay"))]
    {
        window.set_relay_send_source_options(string_model(Vec::<String>::new()));
        window.set_relay_receive_sink_options(string_model(Vec::<String>::new()));
        window.set_relay_send_source_index(0);
        window.set_relay_receive_sink_index(0);
    }
    window.set_effects(ModelRc::from(Rc::new(VecModel::from(effect_rows(
        &application.source,
        &application.i18n,
    )))));
    window.set_effect_options(ModelRc::from(Rc::new(VecModel::from(effect_options(
        &application.source,
    )))));
    window.set_effect_configuring(application.effect_draft_id.is_some());
    window.set_effect_setup_enabled(application.effect_draft_enabled);
    window.set_effect_setup_parameters(ModelRc::from(Rc::new(VecModel::from(effect_setup_rows(
        &application.source,
        application.effect_draft_id.as_deref(),
        &application.effect_draft_parameters,
    )))));
    window.set_rules(ModelRc::from(Rc::new(VecModel::from(rule_rows(
        &application.patchbay,
    )))));
    let (undo_history, redo_history) = application.history();
    window.set_undo_history(ModelRc::from(Rc::new(VecModel::from(history_rows(
        undo_history,
    )))));
    window.set_redo_history(ModelRc::from(Rc::new(VecModel::from(history_rows(
        redo_history,
    )))));
    window.set_effects_available(application.source.supports_effect_nodes());
    window.set_relay_rows(ModelRc::from(Rc::new(VecModel::from(relay_rows(
        application,
        &application.i18n,
    )))));
    #[cfg(feature = "relay")]
    {
        let relay_status = application.source.relay_status();
        window.set_relay_available(application.source.relay_available());
        window.set_relay_host_active(relay_status.host_active);
        window.set_relay_host_endpoint(SharedString::from(relay_host_endpoint(
            application,
            relay_status.host_port,
        )));
        let payload = relay_qr_payload(application).unwrap_or_default();
        window.set_relay_qr_payload(SharedString::from(payload.clone()));
        window.set_relay_qr_image(qr_image(&payload));
        if let Some(pending) = &application.relay_pending_enrollment {
            window.set_relay_pending_active(true);
            window.set_relay_pending_peer_name(SharedString::from(pending.peer_name.clone()));
            window.set_relay_pending_peer_addr(SharedString::from(pending.peer_addr.clone()));
            window.set_relay_pending_peer_id(SharedString::from(pending.peer_id.clone()));
        } else {
            window.set_relay_pending_active(false);
            window.set_relay_pending_peer_name(SharedString::new());
            window.set_relay_pending_peer_addr(SharedString::new());
            window.set_relay_pending_peer_id(SharedString::new());
        }
        window.set_relay_is_connecting(application.relay_connecting.is_some());
        window.set_relay_direction_switching(application.relay_direction_switch.is_some());
    }
    #[cfg(not(feature = "relay"))]
    {
        window.set_relay_available(false);
        window.set_relay_host_active(false);
        window.set_relay_host_endpoint(SharedString::new());
        window.set_relay_qr_payload(SharedString::new());
        window.set_relay_qr_image(Image::default());
    }
    application.snapshot = snapshot;
}

/// Replacing a Slint model invalidates its repeated component instances. Update
/// stable rows in place so the 50ms refresh timer cannot cancel pointer capture
/// between mouse-down and release. Defer structural changes during a drag.
/// Push fresh rows into the model, returning whether they were applied. A
/// gesture in flight keeps the current rows so the pointer cannot lose the
/// component it is dragging.
fn sync_node_rows(window: &MainWindow, nodes: &VecModel<NodeRow>, rows: Vec<NodeRow>) -> bool {
    let stable_shape = nodes.row_count() == rows.len()
        && rows.iter().enumerate().all(|(index, row)| {
            nodes
                .row_data(index)
                .is_some_and(|current| current.id == row.id)
        });
    if stable_shape {
        for (index, row) in rows.into_iter().enumerate() {
            nodes.set_row_data(index, row);
        }
        true
    } else if !window.get_graph_node_dragging() {
        nodes.set_vec(rows);
        true
    } else {
        false
    }
}

/// Ask the backend for the audio state and capability of every node it owns.
///
/// This runs once per sync rather than per node per frame; native drivers
/// answer from a snapshot they refreshed on their own worker.
fn read_backend_profiles(source: &ApplicationDriver) -> BTreeMap<NodeId, NodeBackendProfile> {
    source
        .graph()
        .nodes
        .keys()
        .map(|node_id| {
            let state = source.node_audio_state(*node_id).unwrap_or_default();
            let capabilities = source.node_capabilities(*node_id);
            (
                *node_id,
                NodeBackendProfile {
                    state,
                    capabilities,
                    connectable: source.node_connectable(*node_id),
                },
            )
        })
        .collect()
}

fn node_row(node: &NodeView, i18n: &I18n) -> NodeRow {
    NodeRow {
        id: node.id,
        node_title: SharedString::from(compact_label(&display_node_name(&node.title, i18n), 22)),
        node_subtitle: SharedString::from(localized_node_type(i18n, node.node_type)),
        x: node.position[0],
        y: node.position[1],
        width: node.width,
        height: node.height,
        selected: node.selected,
        collapsed: node.collapsed,
        thumbnail: node.thumbnail,
        // Typography is applied by the shared Slint theme, not duplicated in
        // each projected row.
        font_scale: 1.0,
        accent: color(
            node.appearance
                .color
                .or_else(|| node.ports.first().map(|port| port.color))
                .unwrap_or_else(|| node_type_color(node.node_type)),
        ),
        has_audio_controls: node.has_audio_controls,
        has_audio_panel: node.has_audio_panel,
        has_meter: node.audio.capabilities.has_any_meter(),
        meter_rms: node.meter.rms,
        meter_peak: node.meter.peak,
        meter_peak_position: meter_fraction(node.meter.peak),
        meter_rms_position: meter_fraction(node.meter.rms),
        meter_peak_supported: node.audio.capabilities.meter_peak,
        meter_rms_supported: node.audio.capabilities.meter_rms,
        meter_available: matches!(node.meter.state, MeterState::Live | MeterState::Demo),
        meter_label: SharedString::from(localized_meter_label(i18n, node.meter.state)),
        // Drawn straight from what the backend reported. When it reported
        // nothing the track sits at zero and `audio_volume_known` is false, so
        // the card can show that it has not read a level rather than implying
        // one the system does not actually have.
        audio_volume_position: node
            .audio
            .state
            .volume
            .map(|volume| track_position_from_volume(volume, node.audio.capabilities.volume_max))
            .unwrap_or(0.0),
        audio_volume_known: node.audio.state.volume.is_some(),
        audio_muted: node.audio.state.muted.unwrap_or(false),
        audio_mute_known: node.audio.state.muted.is_some(),
        audio_volume_enabled: node.audio.capabilities.volume_write,
        audio_mute_enabled: node.audio.capabilities.mute_write,
        ports: ModelRc::from(Rc::new(VecModel::from(
            node.ports
                .iter()
                .enumerate()
                .map(|(index, port)| {
                    let is_output = port.direction != pw_graph_core::Direction::Sink;
                    let (pin_x, pin_y) =
                        canvas::pin_offset(node.width, index, node.has_audio_panel, is_output);
                    PortRow {
                        id: port.pin_id,
                        label: SharedString::from(display_port_name(&port.label, i18n)),
                        direction: if is_output { 1 } else { 0 },
                        color: color(port.color),
                        row_y: canvas::port_row_top(index, node.has_audio_panel),
                        pin_x,
                        pin_y,
                    }
                })
                .collect::<Vec<_>>(),
        ))),
    }
}

/// Rebuild the world-space cache the canvas hit-tests and draws against.
fn rebuild_geometry(
    geometry: &Rc<RefCell<CanvasGeometry>>,
    snapshot: &GraphSnapshot,
    connect_mode: ConnectMode,
) {
    let mut node_geometry = Vec::with_capacity(snapshot.nodes.len());
    let mut pin_geometry = Vec::new();
    for node in &snapshot.nodes {
        let pins_visible = !node.collapsed && !node.thumbnail;
        // Asked once per node rather than per pin: every port of a node is
        // owned by the same backend.
        let connectable = node.connectable;
        node_geometry.push(NodeGeometry {
            id: node.id,
            x: node.position[0],
            y: node.position[1],
            width: node.width,
            height: node.height,
            selected: node.selected,
            pins_visible,
        });
        for (index, port) in node.ports.iter().enumerate() {
            let is_output = port.direction != Direction::Sink;
            let (offset_x, offset_y) =
                canvas::pin_offset(node.width, index, node.has_audio_panel, is_output);
            pin_geometry.push(PinGeometry {
                pin_id: port.pin_id,
                node_id: node.id,
                is_output,
                x: node.position[0] + offset_x,
                y: node.position[1] + offset_y,
                visible: pins_visible,
                node_selected: node.selected,
                connectable,
            });
        }
    }
    let link_geometry = snapshot
        .links
        .iter()
        .map(|link| LinkGeometry {
            id: link.id,
            start_pin: link.start_pin_id,
            end_pin: link.end_pin_id,
        })
        .collect();
    geometry.borrow_mut().replace(
        node_geometry,
        pin_geometry,
        link_geometry,
        connect_mode == ConnectMode::Easy,
    );
}

/// Bounding box of every card, used to frame the minimap.
fn graph_bounds(snapshot: &GraphSnapshot) -> [f32; 4] {
    let mut bounds = [f32::MAX, f32::MAX, f32::MIN, f32::MIN];
    for node in &snapshot.nodes {
        bounds[0] = bounds[0].min(node.position[0]);
        bounds[1] = bounds[1].min(node.position[1]);
        bounds[2] = bounds[2].max(node.position[0] + node.width);
        bounds[3] = bounds[3].max(node.position[1] + node.height);
    }
    if snapshot.nodes.is_empty() {
        return [0.0, 0.0, 1600.0, 1200.0];
    }
    bounds
}

pub(crate) fn shortcut_rows(i18n: &I18n, query: &str) -> Vec<ShortcutRow> {
    let query = query.trim().to_ascii_lowercase();
    crate::shortcuts::SHORTCUTS
        .iter()
        .filter_map(|shortcut| {
            let keys = crate::shortcuts::display_keys(shortcut.keys);
            let description = i18n.text(shortcut.description);
            (query.is_empty()
                || keys.to_ascii_lowercase().contains(&query)
                || description.to_ascii_lowercase().contains(&query))
            .then(|| ShortcutRow {
                keys: SharedString::from(keys),
                description: SharedString::from(description),
            })
        })
        .collect()
}

fn history_rows(entries: Vec<String>) -> Vec<HistoryRow> {
    entries
        .into_iter()
        .map(|description| HistoryRow {
            description: SharedString::from(description),
        })
        .collect()
}

fn link_row(link: &LinkView) -> LinkRow {
    LinkRow {
        id: link.id,
        color: color(link.color),
        selected: link.selected,
    }
}

pub(crate) fn rule_rows(patchbay: &Patchbay) -> Vec<RuleRow> {
    patchbay
        .connections
        .iter()
        .enumerate()
        .map(|(index, rule)| RuleRow {
            index: index as i32,
            output: SharedString::from(format!("{} · {}", rule.output_node, rule.output_name)),
            input: SharedString::from(format!("{} · {}", rule.input_node, rule.input_name)),
            output_node: SharedString::from(rule.output_node.clone()),
            output_port: SharedString::from(rule.output_name.clone()),
            input_node: SharedString::from(rule.input_node.clone()),
            input_port: SharedString::from(rule.input_name.clone()),
            pinned: rule.pinned,
        })
        .collect()
}

pub(crate) fn selected_patchbay_path(config: &AppConfig) -> std::path::PathBuf {
    let default_file = config_path("qpwgraph-rs").with_file_name("default.qpwgraph");
    config
        .patchbay_profiles
        .get(&config.active_patchbay_profile)
        .cloned()
        .or_else(|| config.patchbay_path.clone())
        .unwrap_or(default_file)
}

pub(crate) fn profile_options(config: &AppConfig) -> Vec<String> {
    let mut profiles: Vec<_> = config.patchbay_profiles.keys().cloned().collect();
    if profiles.is_empty() {
        profiles.push("default".into());
    }
    if !profiles
        .iter()
        .any(|profile| profile == &config.active_patchbay_profile)
    {
        profiles.push(config.active_patchbay_profile.clone());
        profiles.sort();
    }
    profiles
}

pub(crate) fn profile_index(config: &AppConfig) -> i32 {
    profile_options(config)
        .iter()
        .position(|profile| profile == &config.active_patchbay_profile)
        .unwrap_or(0) as i32
}

pub(crate) fn recent_patchbay_paths(config: &AppConfig) -> Vec<String> {
    config
        .recent_patchbay_paths
        .iter()
        .map(|path| path.display().to_string())
        .collect()
}

pub(crate) fn string_model(values: impl IntoIterator<Item = String>) -> ModelRc<SharedString> {
    ModelRc::from(Rc::new(VecModel::from(
        values
            .into_iter()
            .map(SharedString::from)
            .collect::<Vec<_>>(),
    )))
}

#[cfg(feature = "relay")]
fn relay_send_source_options(application: &Application) -> Vec<(String, String)> {
    let mut options = application
        .source
        .relay_send_sources()
        .into_iter()
        .map(|endpoint| (endpoint.id, endpoint.name))
        .collect::<Vec<_>>();
    if options.is_empty() {
        options.push((
            "default-input".into(),
            application.i18n.text("relay.default_input"),
        ));
    }
    append_unavailable_selector(
        &mut options,
        &application.config.relay_send_source,
        application,
    );
    options
}

#[cfg(feature = "relay")]
fn relay_receive_sink_options(application: &Application) -> Vec<(String, String)> {
    let mut options = application
        .source
        .relay_receive_sinks()
        .into_iter()
        .map(|endpoint| (endpoint.id, endpoint.name))
        .collect::<Vec<_>>();
    if options.is_empty() {
        options.push((
            "default-output".into(),
            application.i18n.text("relay.default_output"),
        ));
    }
    append_unavailable_selector(
        &mut options,
        &application.config.relay_receive_sink,
        application,
    );
    options
}

#[cfg(feature = "relay")]
fn append_unavailable_selector(
    options: &mut Vec<(String, String)>,
    configured: &str,
    application: &Application,
) {
    if !configured.is_empty() && !options.iter().any(|(id, _)| id == configured) {
        options.push((
            configured.to_owned(),
            application.i18n.format(
                "relay.endpoint_unavailable",
                &[("id", configured.to_owned())],
            ),
        ));
    }
}

#[cfg(feature = "relay")]
fn relay_selector_index(current: &str, options: &[(String, String)]) -> i32 {
    options
        .iter()
        .position(|(id, _)| id == current)
        .unwrap_or(0) as i32
}
