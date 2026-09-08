use crate::model::resolve_drag_delta;
use pw_graph_command::MoveNodesCommand;
use slint::VecModel;
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use super::actions::handle_action;
use super::app::{set_connection_feedback, toast_visible, Application, UiEvent};
use super::config::{autosave_config, read_window_state};
use super::connections::{
    easy_connect_from_pin, easy_connect_nodes, handle_link_requested, handle_link_rerouted,
};
use super::meters::refresh_meters;
use super::models::{shortcut_rows, sync_meter_rows, sync_models, vec_model_rows_equal};
use super::relay::{poll_relay_events, poll_relay_usb_hotplug};
use super::utils::volume_from_track_position;
use super::{CanvasGeometry, LinkRow, MainWindow, MinimapNode, NodeRow, ShortcutRow};

/// Slower cadence for the full model sync. Meters still refresh every 50 ms
/// tick through the light path, but topology, selection, config, relay and
/// list models only rebuild when something changed or this interval elapses.
const FULL_SYNC_INTERVAL: Duration = Duration::from_millis(500);

#[allow(clippy::too_many_arguments)]
pub(crate) fn pump(
    window: &MainWindow,
    app: &Rc<RefCell<Application>>,
    nodes: &Rc<VecModel<NodeRow>>,
    links: &Rc<VecModel<LinkRow>>,
    minimap_nodes: &Rc<VecModel<MinimapNode>>,
    shortcuts: &Rc<VecModel<ShortcutRow>>,
    events: &Rc<RefCell<Vec<UiEvent>>>,
    geometry: &Rc<RefCell<CanvasGeometry>>,
    geometry_version: &Rc<Cell<i32>>,
) {
    let had_events = !events.borrow().is_empty();
    let pending = coalesce_audio_volume_events(std::mem::take(&mut *events.borrow_mut()));
    let mut application = app.borrow_mut();
    // Cheap view inputs that must react immediately (typing in the search
    // box, zooming) are captured before the window state is read back.
    let search_before = application.view.search_query.clone();
    let zoom_before = application.view.zoom;
    let pan_before = application.view.pan;
    let thumbnail_before = application.view.thumbnail_mode;
    let toast_before = toast_visible(&application);
    read_window_state(window, &mut application);
    let view_changed = application.view.search_query != search_before
        || application.view.zoom != zoom_before
        || application.view.pan != pan_before
        || application.view.thumbnail_mode != thumbnail_before;
    for event in pending {
        process_event(window, &mut application, event);
    }
    poll_relay_usb_hotplug(&mut application);
    poll_relay_events(&mut application);
    let mut graph_changed = application.source.graph_dirty();
    if graph_changed || application.last_refresh.elapsed() >= refresh_interval(&application) {
        if let Err(error) = application.source.refresh_if_needed() {
            application.status = application.tf("status.refresh_failed", &[("error", error)]);
        } else {
            application.last_refresh = Instant::now();
            graph_changed = true;
        }
    }
    refresh_meters(window, &mut application);
    // A toast expiring only flips a boolean; without this the message would
    // linger until the next slow sync.
    let toast_changed = toast_visible(&application) != toast_before
        || window.get_toast_visible() != toast_visible(&application);
    let full_sync_due = application.last_full_sync.elapsed() >= FULL_SYNC_INTERVAL
        || application.snapshot.nodes.is_empty();
    if had_events || view_changed || graph_changed || toast_changed || full_sync_due {
        autosave_config(&mut application);
        sync_shortcuts(window, &application, shortcuts);
        sync_models(
            window,
            &mut application,
            nodes,
            links,
            minimap_nodes,
            geometry,
            geometry_version,
        );
        application.last_full_sync = Instant::now();
    } else {
        sync_meter_rows(nodes, &application);
    }
}

/// Rebuild the shortcut list only when its query or language changed, not on
/// every 50 ms tick.
fn sync_shortcuts(
    window: &MainWindow,
    application: &Application,
    shortcuts: &Rc<VecModel<ShortcutRow>>,
) {
    let rows = shortcut_rows(&application.i18n, window.get_shortcut_search().as_str());
    if !vec_model_rows_equal(shortcuts, &rows) {
        shortcuts.set_vec(rows);
    }
}

/// How often to re-read the graph when nothing has reported a change.
///
/// A backend that watches its own registry tells us when the topology moves,
/// so the timer is only a safety net against a missed notification. Polling it
/// twice a second re-enumerated every endpoint and session continuously for no
/// reason. A backend that cannot report changes still has to be polled.
const RECONCILE_INTERVAL: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(500);

fn refresh_interval(application: &Application) -> Duration {
    if application.source.reports_graph_changes() {
        RECONCILE_INTERVAL
    } else {
        POLL_INTERVAL
    }
}

pub(crate) fn coalesce_audio_volume_events(pending: Vec<UiEvent>) -> Vec<UiEvent> {
    let mut compacted = Vec::with_capacity(pending.len());
    let mut volume_indices = BTreeMap::<i32, usize>::new();
    for event in pending {
        match event {
            UiEvent::SetAudioVolume(id, position) => {
                if let Some(index) = volume_indices.get(&id).copied() {
                    compacted[index] = UiEvent::SetAudioVolume(id, position);
                } else {
                    volume_indices.insert(id, compacted.len());
                    compacted.push(UiEvent::SetAudioVolume(id, position));
                }
            }
            event => compacted.push(event),
        }
    }
    compacted
}

pub(crate) fn process_event(window: &MainWindow, application: &mut Application, event: UiEvent) {
    match event {
        UiEvent::Action(action) => handle_action(window, application, &action),
        UiEvent::SelectNode(id, shift) => application.view.select_node(id, shift),
        UiEvent::SelectLink(id, shift) => application.view.select_link(id, shift),
        UiEvent::ClearSelection => application.view.clear_selection(),
        UiEvent::SelectBox(x, y, width, height, shift) => {
            application
                .view
                .select_box(&application.snapshot, x, y, width, height, shift)
        }
        UiEvent::LinkRequested(start, end) => handle_link_requested(application, start, end),
        UiEvent::LinkRerouted(link, pin) => handle_link_rerouted(application, link, pin),
        UiEvent::LinkCancelled => {
            application.pending_connection_pin = None;
            set_connection_feedback(
                application,
                application.t("status.connection_cancelled"),
                false,
            );
        }
        UiEvent::NodeConnectDropped(source, x, y, target_pin) => {
            application.pending_connection_pin = None;
            easy_connect_nodes(application, source, x, y, target_pin)
        }
        UiEvent::LinkDropped(source_pin, x, y) => {
            application.pending_connection_pin = None;
            easy_connect_from_pin(application, source_pin, x, y)
        }
        UiEvent::ToggleCollapse(id) => {
            application
                .view
                .toggle_local_collapse(id, &application.snapshot);
            application.status = application.t("status.node_expansion_changed");
        }
        UiEvent::DragCommitted(id, dx, dy) => {
            let Some(dragged) = application.view.ids.node_id(id) else {
                return;
            };
            let selected = if application.view.selected_nodes.contains(&dragged) {
                application.view.selected_nodes.clone()
            } else {
                std::collections::BTreeSet::from([dragged])
            };
            let before: Vec<_> = application
                .snapshot
                .nodes
                .iter()
                .filter(|node| selected.contains(&node.node_id))
                .map(|node| (node.node_id, node.position))
                .collect();
            let resolved = resolve_drag_delta(
                &application.snapshot,
                &selected,
                [dx, dy],
                application.config.repel_overlapping_nodes,
            );
            let after: Vec<_> = before
                .iter()
                .map(|(node, position)| {
                    (
                        *node,
                        [position[0] + resolved[0], position[1] + resolved[1]],
                    )
                })
                .collect();
            if before == after {
                return;
            }
            match application.commands.execute(
                Box::new(MoveNodesCommand::new(before, after)),
                &mut application.source,
            ) {
                Ok(()) => {
                    application.view.move_selected(
                        id,
                        resolved[0],
                        resolved[1],
                        &application.snapshot,
                    );
                    application.status = application.t("status.node_moved");
                }
                Err(error) => {
                    application.status =
                        application.tf("status.layout_failed", &[("error", error.to_string())]);
                }
            }
        }
        UiEvent::SetAudioVolume(id, position) => {
            if let Some(node_id) = application.view.ids.node_id(id) {
                let position = position.clamp(0.0, 1.0);
                // The fader's top of scale is whatever this node accepts, so a
                // backend clamped at unity never reports a boost it discarded.
                let max_volume = application.source.node_capabilities(node_id).volume_max;
                let volume = volume_from_track_position(position, max_volume);
                // The backend owns the value; the next sync reads back whatever
                // it actually applied, so nothing is cached here.
                match application.source.set_node_volume(node_id, volume) {
                    Ok(()) => {
                        application.status = application.tf(
                            "status.node_volume_changed",
                            &[("volume", format!("{:.0}%", volume * 100.0))],
                        );
                    }
                    Err(error) => {
                        application.status =
                            application.tf("status.node_control_failed", &[("error", error)])
                    }
                }
            }
        }
        UiEvent::ToggleAudioMute(id) => {
            if let Some(node_id) = application.view.ids.node_id(id) {
                // Toggle against the backend's own reading. A node whose mute
                // state has never been read is treated as unmuted, so the first
                // toggle mutes it -- the same thing the user just asked for.
                let muted = !application
                    .source
                    .node_audio_state(node_id)
                    .ok()
                    .and_then(|state| state.muted)
                    .unwrap_or(false);
                match application.source.set_node_mute(node_id, muted) {
                    Ok(()) => {
                        application.status = application.tf(
                            "status.node_mute_changed",
                            &[(
                                "state",
                                application.t(if muted {
                                    "canvas.muted"
                                } else {
                                    "canvas.unmuted"
                                }),
                            )],
                        );
                    }
                    Err(error) => {
                        application.status =
                            application.tf("status.node_control_failed", &[("error", error)])
                    }
                }
            }
        }
    }
}
