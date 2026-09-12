use crate::model::node_layout_key;
use crate::model::{ConnectMode, MediaFilter};
use pw_graph_command::{DisconnectAllCommand, MoveNodesCommand};
use pw_graph_core::{decode_backend_local_id, decode_backend_namespace, NodeAppearance};
use std::fmt::Write as FmtWrite;
use std::time::Instant;

use super::app::Application;
use super::config::save_config;
use super::connections::{delete_selected_connections, disconnect_selected_node};
use super::effects::{cancel_effect_setup, close_effect_diagnostics, remove_effect};
use super::patchbay::{
    activate_patchbay, add_rule_from_selection, begin_rule_edit, cancel_rule_edit,
    choose_patchbay_directory, close_patchbay_diagnostics, load_patchbay, load_recent_patchbay,
    open_patchbay_diagnostics, remove_rule, save_patchbay, save_profile, save_rule, select_profile,
    snapshot_patchbay, toggle_rule_pin,
};
use super::recorders::{
    choose_recording_directory, close_recovery_dialog, create_recorder,
    discard_recovered_recording, save_recovered_recording,
};
use super::relay::{
    accept_pending_enrollment, cancel_relay_connect, connect_relay, disconnect_relay,
    forget_trusted_peer, regenerate_host_pin, reject_pending_enrollment, relay_host_active,
    relay_qr_payload, start_relay_discovery, start_relay_host, stop_relay_discovery,
    stop_relay_host,
};
use super::MainWindow;

pub(crate) fn handle_action(window: &MainWindow, application: &mut Application, action: &str) {
    match action {
        "refresh" => match application.source.refresh() {
            Ok(()) => {
                application.last_refresh = Instant::now();
                application.status = application.tf(
                    "status.refreshed",
                    &[("count", application.source.graph().nodes.len().to_string())],
                );
            }
            Err(error) => {
                application.status = application.tf("status.refresh_failed", &[("error", error)]);
            }
        },
        "zoom-in" => application.view.zoom = (application.view.zoom * 1.1).clamp(0.35, 2.5),
        "zoom-out" => application.view.zoom = (application.view.zoom / 1.1).clamp(0.35, 2.5),
        "toggle-thumbnail" => {
            application.view.thumbnail_mode = !application.view.thumbnail_mode;
            application.status = application.t("status.thumbnail_changed");
        }
        "toggle-minimap" => application.view.minimap_visible = !application.view.minimap_visible,
        "toggle-sort-type" => {
            application.view.sort_ports_by_name = !application.view.sort_ports_by_name;
            application.status = application.tf(
                "status.sort_changed",
                &[(
                    "sort",
                    application.t(if application.view.sort_ports_by_name {
                        "sort.name"
                    } else {
                        "sort.id"
                    }),
                )],
            );
        }
        "toggle-sort-order" => {
            application.view.sort_ports_descending = !application.view.sort_ports_descending;
            application.status = application.tf(
                "status.sort_order_changed",
                &[(
                    "order",
                    application.t(if application.view.sort_ports_descending {
                        "sort.descending"
                    } else {
                        "sort.ascending"
                    }),
                )],
            );
        }
        "toggle-connect-mode" => {
            if !application.source.capabilities().connect {
                application.status = application.t("status.connections_unavailable");
                return;
            }
            application.view.connect_mode = match application.view.connect_mode {
                ConnectMode::Advanced => ConnectMode::Easy,
                ConnectMode::Easy => ConnectMode::Advanced,
            };
            application.status = if application.view.connect_mode == ConnectMode::Easy {
                application.t("connect.easy")
            } else {
                application.t("connect.advanced")
            };
        }
        "filter-all" => application.view.media_filter = MediaFilter::All,
        "filter-audio" => application.view.media_filter = MediaFilter::Audio,
        "filter-video" => application.view.media_filter = MediaFilter::Video,
        "filter-midi" => application.view.media_filter = MediaFilter::Midi,
        "cycle-filter" => {
            application.view.media_filter = match application.view.media_filter {
                MediaFilter::All => MediaFilter::Audio,
                MediaFilter::Audio => MediaFilter::Video,
                MediaFilter::Video => MediaFilter::Midi,
                MediaFilter::Midi => MediaFilter::All,
            }
        }
        "arrange" => {
            let before: Vec<_> = application
                .source
                .graph()
                .nodes
                .values()
                .map(|node| (node.id, node.position))
                .collect();
            let defaults = application.source.graph().default_node_positions();
            let after: Vec<_> = before
                .iter()
                .map(|(node, position)| (*node, defaults.get(node).copied().unwrap_or(*position)))
                .collect();
            // How many nodes the arrange actually moves. Reporting the whole
            // graph's node count put a number on screen that contradicted the
            // status bar beside it, which counts only the nodes the current
            // media filter leaves visible.
            let moved = before
                .iter()
                .zip(after.iter())
                .filter(|((_, from), (_, to))| from != to)
                .count();
            if before != after {
                match application.commands.execute(
                    Box::new(MoveNodesCommand::new(before, after)),
                    &mut application.source,
                ) {
                    Ok(()) => {
                        let _ = application.source.refresh();
                        application
                            .view
                            .adopt_backend_positions(application.source.graph());
                        application.status =
                            application.tf("status.arranged", &[("count", moved.to_string())]);
                    }
                    Err(error) => {
                        application.status =
                            application.tf("status.layout_failed", &[("error", error.to_string())]);
                    }
                }
            }
        }
        "preferences" => toggle_overlay(window, Overlay::Preferences),
        "history" => toggle_overlay(window, Overlay::History),
        "shortcuts" => toggle_overlay(window, Overlay::Shortcuts),
        "copy-windows-audio-report" => {
            #[cfg(target_os = "windows")]
            {
                let report = application.source.windows_audio_report();
                match crate::diagnostics::copy_text(&report) {
                    Ok(()) => application.status = application.t("status.windows_report_copied"),
                    Err(error) => {
                        application.status = application
                            .tf("status.windows_report_copy_failed", &[("error", error)]);
                    }
                }
            }
            #[cfg(not(target_os = "windows"))]
            {
                application.status = application.t("status.windows_report_unavailable");
            }
        }
        "effects" => {
            if window.get_show_effects() {
                cancel_effect_setup(window, application);
            }
            toggle_overlay(window, Overlay::Effects);
        }
        "add-recorder" => create_recorder(application),
        "choose-recording-directory" => choose_recording_directory(application),
        "recovery-close" => close_recovery_dialog(application),
        "node-appearance" => open_node_appearance(window, application),
        "node-appearance-close" => window.set_show_node_editor(false),
        "node-identity-debug" => open_node_identity_diagnostics(window, application),
        "node-identity-debug-close" => {
            window.set_show_node_diagnostics(false);
            application.node_debug_report.clear();
        }
        "node-identity-debug-copy" => {
            application.status = application.t("status.node_diagnostics_copied");
        }
        "patchbay-debug" => open_patchbay_diagnostics(window, application),
        "patchbay-debug-close" => close_patchbay_diagnostics(window, application),
        "patchbay-debug-copy" => {
            super::patchbay::copy_patchbay_diagnostics(application);
        }
        "node-appearance-reset-name" => window.set_node_editor_custom_name("".into()),
        "node-appearance-reset-color" => window.set_node_editor_color("".into()),
        "node-appearance-save" => save_node_appearance(window, application),
        "relay" => {
            let show = !window.get_show_relay();
            window.set_show_relay(show);
            close_modals(window);
            if show {
                start_relay_discovery(application);
            } else {
                stop_relay_discovery(application);
            }
        }
        "relay-show-qr" => {
            if relay_qr_payload(application).is_some() {
                window.set_show_qr(true);
            } else {
                application.status = application.t("relay.start_host_first");
            }
        }
        "close-qr" => window.set_show_qr(false),
        "relay-connect-configured" => connect_relay(application, None),
        "relay-connect" => connect_relay(application, None),
        "relay-host-toggle" => {
            if relay_host_active(application) {
                stop_relay_host(application);
            } else {
                start_relay_host(application);
            }
        }
        "relay-host-start" => start_relay_host(application),
        "relay-host-stop" => stop_relay_host(application),
        "toggle-statusbar" => window.set_show_statusbar(!window.get_show_statusbar()),
        "reset-audio" => {
            application.source.reset_meters();
            application.meters.clear();
            application.status = application.t("status.audio_monitoring_reset");
        }
        "escape" => escape_topmost_layer(window, application),
        "save-config" => save_config(application, true),
        "delete-selection" => delete_selection(application),
        "disconnect-node" => disconnect_selected_node(application),
        "disconnect-all" => {
            if !application.source.capabilities().disconnect {
                application.status = application.t("status.connections_unavailable");
                return;
            }
            let count = application
                .source
                .graph()
                .links
                .values()
                .filter(|link| application.source.is_link_mutable(link.id))
                .count();
            if count == 0 {
                application.status = application.t("status.no_links");
                return;
            }
            let keys = application
                .source
                .graph()
                .links
                .values()
                .filter(|link| application.source.is_link_mutable(link.id))
                .filter_map(|link| {
                    application
                        .source
                        .graph()
                        .port_key(link.output_port)
                        .zip(application.source.graph().port_key(link.input_port))
                })
                .collect::<Vec<_>>();
            match application.commands.execute(
                Box::new(DisconnectAllCommand::new()),
                &mut application.source,
            ) {
                Ok(()) => {
                    application.view.clear_selection();
                    application.remove_patchbay_connections(&keys);
                    application.sync_patchbay_connections();
                    application.autosave_patchbay();
                    application.status =
                        application.tf("status.disconnected_all", &[("count", count.to_string())]);
                }
                Err(error) => {
                    application.status =
                        application.tf("status.disconnect_failed", &[("error", error.to_string())]);
                }
            }
        }
        "undo" => {
            let before = application.live_connection_keys();
            let changed = match application.commands.undo(&mut application.source) {
                Ok(true) => {
                    application.status = application.t("status.undo_complete");
                    true
                }
                Ok(false) => {
                    application.status = application.t("status.nothing_to_undo");
                    false
                }
                Err(error) => {
                    application.status =
                        application.tf("status.undo_failed", &[("error", error.to_string())]);
                    false
                }
            };
            let _ = application.source.refresh();
            if changed {
                application
                    .view
                    .adopt_backend_positions(application.source.graph());
            }
            let after = application.live_connection_keys();
            for pair in before.iter().filter(|pair| !after.contains(pair)) {
                application.remove_patchbay_connections(std::slice::from_ref(pair));
            }
            application.sync_patchbay_connections();
            application.autosave_patchbay();
        }
        "redo" => {
            let before = application.live_connection_keys();
            let changed = match application.commands.redo(&mut application.source) {
                Ok(true) => {
                    application.status = application.t("status.redo_complete");
                    true
                }
                Ok(false) => {
                    application.status = application.t("status.nothing_to_redo");
                    false
                }
                Err(error) => {
                    application.status =
                        application.tf("status.redo_failed", &[("error", error.to_string())]);
                    false
                }
            };
            let _ = application.source.refresh();
            if changed {
                application
                    .view
                    .adopt_backend_positions(application.source.graph());
            }
            let after = application.live_connection_keys();
            for pair in before.iter().filter(|pair| !after.contains(pair)) {
                application.remove_patchbay_connections(std::slice::from_ref(pair));
            }
            application.sync_patchbay_connections();
            application.autosave_patchbay();
        }
        "save-patchbay" => save_patchbay(application),
        "load-patchbay" => load_patchbay(application),
        "activate-patchbay" => activate_patchbay(application),
        "save-profile" => save_profile(application),
        _ if action.strip_prefix("select-profile:").is_some() => {
            if let Some(index) = action
                .strip_prefix("select-profile:")
                .and_then(|value| value.parse::<usize>().ok())
            {
                select_profile(window, application, index);
            }
        }
        _ if action.strip_prefix("recent-patchbay:").is_some() => {
            if let Some(index) = action
                .strip_prefix("recent-patchbay:")
                .and_then(|value| value.parse::<usize>().ok())
            {
                load_recent_patchbay(application, index);
            }
        }
        _ if action.strip_prefix("recovery-save:").is_some() => {
            if let Some(index) = action
                .strip_prefix("recovery-save:")
                .and_then(|value| value.parse::<usize>().ok())
            {
                save_recovered_recording(application, index);
            }
        }
        _ if action.strip_prefix("recovery-discard:").is_some() => {
            if let Some(index) = action
                .strip_prefix("recovery-discard:")
                .and_then(|value| value.parse::<usize>().ok())
            {
                discard_recovered_recording(application, index);
            }
        }
        "choose-patchbay-directory" => choose_patchbay_directory(application),
        "add-rule" => add_rule_from_selection(window, application),
        "snapshot-patchbay" => snapshot_patchbay(application),
        _ if action.strip_prefix("rule-edit:").is_some() => {
            if let Some(index) = action
                .strip_prefix("rule-edit:")
                .and_then(|value| value.parse::<usize>().ok())
            {
                begin_rule_edit(window, application, index);
            }
        }
        "rule-edit-save" => save_rule(window, application),
        "rule-edit-cancel" => cancel_rule_edit(window, application),
        _ if action.strip_prefix("rule-remove:").is_some() => {
            if let Some(index) = action
                .strip_prefix("rule-remove:")
                .and_then(|value| value.parse::<usize>().ok())
            {
                remove_rule(application, index);
            }
        }
        _ if action.strip_prefix("rule-pin:").is_some() => {
            if let Some(index) = action
                .strip_prefix("rule-pin:")
                .and_then(|value| value.parse::<usize>().ok())
            {
                toggle_rule_pin(application, index);
            }
        }
        _ if action.strip_prefix("relay-connect:").is_some() => {
            let target = action.strip_prefix("relay-connect:").unwrap_or_default();
            connect_relay(application, Some(target));
        }
        _ if action.strip_prefix("relay-disconnect:").is_some() => {
            let session = action
                .strip_prefix("relay-disconnect:")
                .and_then(|value| value.parse::<u64>().ok());
            disconnect_relay(application, session);
        }
        _ if action.strip_prefix("relay-forget:").is_some() => {
            let peer_id = action.strip_prefix("relay-forget:").unwrap_or_default();
            forget_trusted_peer(application, peer_id);
        }
        "relay-enrollment-accept" => accept_pending_enrollment(application),
        "relay-enrollment-reject" => reject_pending_enrollment(application),
        "relay-cancel-connect" => cancel_relay_connect(application),
        "relay-regenerate-pin" => regenerate_host_pin(application),
        _ => {
            application.status =
                application.tf("status.unknown_action", &[("action", action.to_owned())]);
        }
    }
    if application.debug {
        eprintln!("[qpwgraph-rs] {}", application.status);
    }
}

fn open_node_identity_diagnostics(window: &MainWindow, application: &mut Application) {
    let Some(node_id) = application.view.selected_nodes.iter().next().copied() else {
        application.status = application.t("status.select_node_before_debug");
        return;
    };
    let graph = application.source.graph();
    let Some(node) = graph.node(node_id) else {
        application.status = application.t("status.select_node_before_debug");
        return;
    };
    let identity = node.matching_identity();
    let namespace = decode_backend_namespace(node.id.0);
    let mut report = String::new();
    let _ = writeln!(report, "Node identity diagnostics");
    let _ = writeln!(report, "graph_id={}", node.id.0);
    let _ = writeln!(report, "backend={namespace:?}");
    let _ = writeln!(
        report,
        "current_global_id={}",
        decode_backend_local_id(node.id.0)
    );
    let _ = writeln!(report, "node_type={:?}", node.node_type);
    let _ = writeln!(report, "node.name={}", node.name);
    let _ = writeln!(report, "node.description={:?}", identity.description);
    let _ = writeln!(report, "object.serial={:?}", identity.object_serial);
    let _ = writeln!(report, "client.id={:?}", identity.client_id);
    let _ = writeln!(report, "client.name={:?}", identity.client_name);
    let _ = writeln!(report, "client.api={:?}", identity.client_api);
    let _ = writeln!(report, "application.id={:?}", identity.application_id);
    let _ = writeln!(report, "application.name={:?}", identity.application_name);
    let _ = writeln!(
        report,
        "application.process.binary={:?}",
        identity.process_binary
    );
    let _ = writeln!(report, "media.role={:?}", identity.media_role);
    let _ = writeln!(report, "media.name={:?}", identity.media_name);
    let _ = writeln!(report, "object.path={:?}", identity.object_path);
    let _ = writeln!(
        report,
        "effect.instance_id={:?}",
        identity.effect_instance_id
    );
    let _ = writeln!(report, "\nPorts:");
    for port_id in &node.ports {
        let Some(port) = graph.port(*port_id) else {
            continue;
        };
        let _ = writeln!(
            report,
            "  graph_id={} current_global_id={} name={} channel={:?} direction={:?} type={:?}",
            port.id.0,
            decode_backend_local_id(port.id.0),
            port.name,
            port.channel,
            port.direction,
            port.port_type,
        );
        if let Some(key) = graph.port_key(port.id) {
            let _ = writeln!(
                report,
                "    selector_resolution={}",
                graph.endpoint_resolution_explanation(&key.selector())
            );
        }
    }
    application.node_debug_name = node.name.clone();
    application.node_debug_report = report;
    window.set_show_node_diagnostics(true);
}

fn delete_selection(application: &mut Application) {
    if !application.view.selected_links.is_empty() {
        delete_selected_connections(application);
        return;
    }

    let recorder_ids = application
        .view
        .selected_nodes
        .iter()
        .filter_map(|node_id| {
            application
                .recorders
                .values()
                .find(|recorder| recorder.node_id == *node_id)
                .map(|recorder| recorder.id)
        })
        .collect::<Vec<_>>();
    if !recorder_ids.is_empty() {
        for id in recorder_ids {
            match application.source.remove_recorder(id) {
                Ok(()) => {
                    application.recorders.remove(&id);
                    application.pending_recorder_stops.remove(&id);
                }
                Err(error) => {
                    application.status =
                        application.tf("status.recorder_failed", &[("error", error)]);
                    return;
                }
            }
        }
        application.view.clear_selection();
        application.mark_patchbay_graph_dirty();
        return;
    }

    let effect_ids = application
        .view
        .selected_nodes
        .iter()
        .filter_map(|node_id| {
            application
                .source
                .effect_instances()
                .into_iter()
                .find(|effect| effect.node_id == *node_id)
                .map(|effect| effect.config.instance_id)
        })
        .collect::<Vec<_>>();
    if effect_ids.is_empty() {
        delete_selected_connections(application);
        return;
    }
    for instance_id in effect_ids {
        remove_effect(application, &instance_id);
    }
    application.view.clear_selection();
}

#[derive(Clone, Copy)]
enum Overlay {
    Preferences,
    History,
    Shortcuts,
    Effects,
}

fn toggle_overlay(window: &MainWindow, overlay: Overlay) {
    let currently_open = match overlay {
        Overlay::Preferences => window.get_show_preferences(),
        Overlay::History => window.get_show_history(),
        Overlay::Shortcuts => window.get_show_shortcuts(),
        Overlay::Effects => window.get_show_effects(),
    };
    close_modals(window);
    match overlay {
        Overlay::Preferences => window.set_show_preferences(!currently_open),
        Overlay::History => window.set_show_history(!currently_open),
        Overlay::Shortcuts => window.set_show_shortcuts(!currently_open),
        Overlay::Effects => window.set_show_effects(!currently_open),
    }
}

/// Escape cancels the topmost active layer and nothing else.
///
/// It used to close every overlay at once, which meant dismissing a QR code
/// also tore down the relay panel underneath it and stopped discovery. The
/// order below is the layering order on screen, innermost first:
///
/// ```text
/// QR dialog -> patchbay/node diagnostics -> effect diagnostics -> node appearance
/// -> effect setup -> the four modals -> relay panel -> canvas gesture
/// ```
///
/// The canvas gesture is cancelled in `ui/main.slint` before this runs, so an
/// Escape with nothing open still aborts a drag.
fn escape_topmost_layer(window: &MainWindow, application: &mut Application) {
    if window.get_relay_pending_active() {
        super::relay::reject_pending_enrollment(application);
        return;
    }
    if window.get_show_recovery() {
        window.set_show_recovery(false);
        close_recovery_dialog(application);
        return;
    }
    if window.get_show_qr() {
        window.set_show_qr(false);
        return;
    }
    if window.get_show_patchbay_diagnostics() {
        close_patchbay_diagnostics(window, application);
        return;
    }
    if window.get_show_node_diagnostics() {
        window.set_show_node_diagnostics(false);
        application.node_debug_report.clear();
        return;
    }
    if window.get_show_effect_diagnostics() {
        close_effect_diagnostics(window, application);
        return;
    }
    if window.get_show_node_editor() {
        window.set_show_node_editor(false);
        return;
    }
    // Inside the effects dialog a half-filled setup form is its own layer: the
    // first Escape abandons the draft, a second one closes the dialog.
    if window.get_show_effects() && window.get_effect_configuring() {
        cancel_effect_setup(window, application);
        return;
    }
    if window.get_show_effects() {
        // A completed Create action closes the setup subview while its ticket
        // continues in the background. This call only discards an unsubmitted
        // draft; a submitted ticket is cancelled through its own operation
        // card or during shutdown.
        cancel_effect_setup(window, application);
        window.set_show_effects(false);
        return;
    }
    if window.get_show_shortcuts() {
        window.set_show_shortcuts(false);
        return;
    }
    if window.get_show_history() {
        window.set_show_history(false);
        return;
    }
    if window.get_show_preferences() {
        window.set_show_preferences(false);
        return;
    }
    if window.get_show_relay() {
        window.set_show_relay(false);
        stop_relay_discovery(application);
    }
}

fn close_modals(window: &MainWindow) {
    window.set_show_preferences(false);
    window.set_show_history(false);
    window.set_show_shortcuts(false);
    window.set_show_effects(false);
    window.set_show_effect_diagnostics(false);
    window.set_show_patchbay_diagnostics(false);
    window.set_show_node_editor(false);
    window.set_show_node_diagnostics(false);
}

fn open_node_appearance(window: &MainWindow, application: &mut Application) {
    let Some(node_id) = application.view.selected_nodes.iter().next().copied() else {
        application.status = application.t("status.select_node_for_appearance");
        return;
    };
    let Some(node) = application.source.graph().node(node_id) else {
        application.status = application.t("status.node_not_found");
        return;
    };
    let appearance = application
        .view
        .local_appearance(node_id, &application.snapshot)
        .unwrap_or_default();
    window.set_node_editor_node_name(node.name.clone().into());
    window.set_node_editor_custom_name(appearance.custom_name.unwrap_or_default().into());
    window.set_node_editor_color(
        appearance
            .color
            .map(format_color)
            .unwrap_or_default()
            .into(),
    );
    window.set_show_node_editor(true);
}

fn save_node_appearance(window: &MainWindow, application: &mut Application) {
    let Some(node_id) = application.view.selected_nodes.iter().next().copied() else {
        application.status = application.t("status.select_node_for_appearance");
        return;
    };
    let Some(node) = application.source.graph().node(node_id) else {
        application.status = application.t("status.node_not_found");
        return;
    };
    let color_text = window.get_node_editor_color().trim().to_owned();
    let color = if color_text.is_empty() {
        None
    } else {
        match parse_color(&color_text) {
            Some(color) => Some(color),
            None => {
                application.status = application.t("status.invalid_color");
                return;
            }
        }
    };
    let custom_name = match window.get_node_editor_custom_name().trim() {
        "" => None,
        value => Some(value.to_owned()),
    };
    let appearance = NodeAppearance {
        collapsed: application
            .view
            .local_appearance(node_id, &application.snapshot)
            .map(|value| value.collapsed)
            .unwrap_or(false),
        custom_name,
        color,
    };
    application
        .config
        .node_view_by_name
        .insert(node_layout_key(node), appearance.clone());
    application.view.set_local_appearance(node_id, appearance);
    window.set_show_node_editor(false);
    application.status = application.t("status.node_appearance_saved");
}

fn parse_color(value: &str) -> Option<[u8; 4]> {
    let value = value.trim().strip_prefix('#').unwrap_or(value.trim());
    if value.len() != 6 && value.len() != 8 {
        return None;
    }
    let red = u8::from_str_radix(&value[0..2], 16).ok()?;
    let green = u8::from_str_radix(&value[2..4], 16).ok()?;
    let blue = u8::from_str_radix(&value[4..6], 16).ok()?;
    let alpha = if value.len() == 8 {
        u8::from_str_radix(&value[6..8], 16).ok()?
    } else {
        255
    };
    Some([red, green, blue, alpha])
}

fn format_color(color: [u8; 4]) -> String {
    format!(
        "#{:02x}{:02x}{:02x}{:02x}",
        color[0], color[1], color[2], color[3]
    )
}
