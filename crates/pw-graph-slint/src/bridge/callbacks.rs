use crate::canvas::{self, CanvasGeometry, HIT_NODE, HIT_NODE_BODY, PIN_DROP_RADIUS};
use slint::{ComponentHandle, Model, SharedString, VecModel};
use std::cell::RefCell;
use std::collections::BTreeSet;
use std::rc::Rc;

use super::app::UiEvent;
use super::{HitResult, LinkRow, MainWindow, NodeRow};

/// World-space distance between two background grid lines.
pub(crate) const GRID_SPACING: f32 = 24.0;

/// Wire every canvas gesture and every geometry question to the bridge.
/// Kept separate from the rest of the window wiring so the UI tests can drive
/// exactly the same code path the application uses.
pub(crate) fn install_canvas_callbacks(
    window: &MainWindow,
    nodes_source: &Rc<VecModel<NodeRow>>,
    links_source: &Rc<VecModel<LinkRow>>,
    geometry_source: &Rc<RefCell<CanvasGeometry>>,
    events_source: &Rc<RefCell<Vec<UiEvent>>>,
) {
    let events = events_source.clone();
    let nodes = nodes_source.clone();
    let links = links_source.clone();
    let geometry = geometry_source.clone();
    window.on_graph_node_selected(move |id, shift| {
        project_node_selection(&nodes, &links, id, shift);
        sync_geometry_selection(&nodes, &geometry);
        events.borrow_mut().push(UiEvent::SelectNode(id, shift));
    });
    let events = events_source.clone();
    let nodes = nodes_source.clone();
    let links = links_source.clone();
    let geometry = geometry_source.clone();
    window.on_graph_link_selected(move |id, shift| {
        project_link_selection(&nodes, &links, id, shift);
        sync_geometry_selection(&nodes, &geometry);
        events.borrow_mut().push(UiEvent::SelectLink(id, shift));
    });
    let events = events_source.clone();
    let nodes = nodes_source.clone();
    let links = links_source.clone();
    let geometry = geometry_source.clone();
    window.on_graph_selection_cleared(move || {
        clear_model_selection(&nodes, &links);
        sync_geometry_selection(&nodes, &geometry);
        events.borrow_mut().push(UiEvent::ClearSelection);
    });
    let events = events_source.clone();
    let nodes = nodes_source.clone();
    let links = links_source.clone();
    let geometry = geometry_source.clone();
    window.on_graph_box_selected(move |x, y, width, height, shift| {
        project_box_selection(&nodes, &links, &geometry, x, y, width, height, shift);
        sync_geometry_selection(&nodes, &geometry);
        events
            .borrow_mut()
            .push(UiEvent::SelectBox(x, y, width, height, shift));
    });
    let events = events_source.clone();
    let nodes = nodes_source.clone();
    let geometry = geometry_source.clone();
    window.on_graph_node_dragged(move |id, dx, dy| {
        // Apply the move to the rendered rows immediately: the canvas drops
        // its live offset as soon as this returns, so anything slower than
        // synchronous would show the card snapping back for a frame.
        commit_drag(&nodes, id, dx, dy);
        geometry.borrow_mut().translate_selected(id, dx, dy);
        events.borrow_mut().push(UiEvent::DragCommitted(id, dx, dy));
    });
    let events = events_source.clone();
    window.on_graph_link_requested(move |start, end| {
        events.borrow_mut().push(UiEvent::LinkRequested(start, end));
    });
    let events = events_source.clone();
    window.on_graph_link_cancelled(move || {
        events.borrow_mut().push(UiEvent::LinkCancelled);
    });
    let events = events_source.clone();
    let weak_window = window.as_weak();
    window.on_graph_link_dropped(move |pin, x, y| {
        // Easy mode connects whole cards, so a pin drag that lands on a
        // card rather than on its pin is still a connection request.
        let can_connect_through = weak_window.upgrade().is_some_and(|window| {
            window.get_connect_mode() == "easy" || window.get_connect_through()
        });
        events.borrow_mut().push(if can_connect_through {
            UiEvent::LinkDropped(pin, x, y)
        } else {
            UiEvent::LinkCancelled
        });
    });
    let events = events_source.clone();
    let geometry = geometry_source.clone();
    window.on_graph_node_connect_dropped(move |id, x, y| {
        let target_pin = geometry.borrow().find_pin_at(x, y, PIN_DROP_RADIUS);
        events
            .borrow_mut()
            .push(UiEvent::NodeConnectDropped(id, x, y, target_pin));
    });
    let events = events_source.clone();
    window.on_graph_audio_volume_changed(move |id, value| {
        events.borrow_mut().push(UiEvent::SetAudioVolume(id, value));
    });
    let events = events_source.clone();
    window.on_graph_audio_mute_toggled(move |id| {
        events.borrow_mut().push(UiEvent::ToggleAudioMute(id));
    });
    let geometry = geometry_source.clone();
    window.on_graph_hit_test(move |x, y, zoom| {
        let geometry = geometry.borrow();
        let hit = geometry.hit_test(x, y, zoom);
        HitResult {
            kind: hit.kind,
            id: hit.id,
            x: hit.x,
            y: hit.y,
            selected: matches!(hit.kind, HIT_NODE | HIT_NODE_BODY)
                && geometry.node(hit.id).is_some_and(|node| node.selected),
        }
    });
    let geometry = geometry_source.clone();
    window.on_graph_link_path(move |id, _version, drag_x, drag_y| {
        SharedString::from(geometry.borrow().link_path(id, (drag_x, drag_y)))
    });
    let events = events_source.clone();
    window.on_graph_link_rerouted(move |link, pin| {
        events.borrow_mut().push(UiEvent::LinkRerouted(link, pin));
    });
    let geometry = geometry_source.clone();
    window.on_graph_pin_connectable(move |pin| geometry.borrow().pin_connectable(pin));
    let geometry = geometry_source.clone();
    window.on_graph_link_drag_anchor(move |link, x, y| {
        geometry.borrow().link_drag_anchor(link, x, y)
    });
    let geometry = geometry_source.clone();
    window.on_graph_pin_preview_path(move |pin, x, y| {
        SharedString::from(geometry.borrow().preview_path(pin, x, y))
    });
    let geometry = geometry_source.clone();
    window.on_graph_node_preview_path(move |id, x, y| {
        SharedString::from(geometry.borrow().node_preview_path(id, x, y))
    });
    let weak_window = window.as_weak();
    window.on_graph_request_grid(move || {
        if let Some(window) = weak_window.upgrade() {
            window.set_grid_commands(SharedString::from(canvas::grid_commands(
                window.get_canvas_width_(),
                window.get_canvas_height_(),
                window.get_zoom(),
                window.get_pan_x(),
                window.get_pan_y(),
                GRID_SPACING,
            )));
        }
    });

    let events = events_source.clone();
    window.on_graph_node_collapse_toggled(move |node_id| {
        events.borrow_mut().push(UiEvent::ToggleCollapse(node_id));
    });
}

fn set_selection_flags<T: Clone + 'static>(
    model: &VecModel<T>,
    selected_of: impl Fn(&mut T) -> &mut bool,
    flags: &[bool],
) {
    for (index, flag) in flags.iter().enumerate() {
        let Some(mut row) = model.row_data(index) else {
            continue;
        };
        if *selected_of(&mut row) != *flag {
            *selected_of(&mut row) = *flag;
            model.set_row_data(index, row);
        }
    }
}

fn clear_selection_flags<T: Clone + 'static>(
    model: &VecModel<T>,
    selected_of: impl Fn(&mut T) -> &mut bool,
) {
    let flags = vec![false; model.row_count()];
    set_selection_flags(model, selected_of, &flags);
}

pub(crate) fn rows_of<T: Clone + 'static>(model: &VecModel<T>) -> Vec<T> {
    (0..model.row_count())
        .filter_map(|index| model.row_data(index))
        .collect()
}

fn project_node_selection(
    nodes: &Rc<VecModel<NodeRow>>,
    links: &Rc<VecModel<LinkRow>>,
    node_id: i32,
    shift: bool,
) {
    if !shift {
        clear_selection_flags(links, |link| &mut link.selected);
    }
    let flags = canvas::apply_click(
        &rows_of(nodes),
        |node| node.id,
        |node| node.selected,
        node_id,
        shift,
    );
    set_selection_flags(nodes, |node| &mut node.selected, &flags);
}

fn project_link_selection(
    nodes: &Rc<VecModel<NodeRow>>,
    links: &Rc<VecModel<LinkRow>>,
    link_id: i32,
    shift: bool,
) {
    if !shift {
        clear_selection_flags(nodes, |node| &mut node.selected);
    }
    let flags = canvas::apply_click(
        &rows_of(links),
        |link| link.id,
        |link| link.selected,
        link_id,
        shift,
    );
    set_selection_flags(links, |link| &mut link.selected, &flags);
}

fn clear_model_selection(nodes: &Rc<VecModel<NodeRow>>, links: &Rc<VecModel<LinkRow>>) {
    clear_selection_flags(nodes, |node| &mut node.selected);
    clear_selection_flags(links, |link| &mut link.selected);
}

/// Push the rendered selection into the geometry cache. The canvas asks the
/// cache which cards a drag should move, so the two must never disagree.
fn sync_geometry_selection(nodes: &Rc<VecModel<NodeRow>>, geometry: &Rc<RefCell<CanvasGeometry>>) {
    let selected: BTreeSet<i32> = rows_of(nodes)
        .into_iter()
        .filter(|node| node.selected)
        .map(|node| node.id)
        .collect();
    geometry
        .borrow_mut()
        .apply_selection(|id| selected.contains(&id));
}

/// Move the dragged card and everything selected with it.
fn commit_drag(nodes: &Rc<VecModel<NodeRow>>, dragged: i32, dx: f32, dy: f32) {
    for index in 0..nodes.row_count() {
        let Some(mut node) = nodes.row_data(index) else {
            continue;
        };
        if node.selected || node.id == dragged {
            node.x += dx;
            node.y += dy;
            nodes.set_row_data(index, node);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn project_box_selection(
    nodes: &Rc<VecModel<NodeRow>>,
    links: &Rc<VecModel<LinkRow>>,
    geometry: &Rc<RefCell<CanvasGeometry>>,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    shift: bool,
) {
    let (node_hits, link_hits) = {
        let geometry = geometry.borrow();
        (
            geometry
                .nodes_in_box(x, y, width, height)
                .into_iter()
                .collect::<std::collections::HashSet<_>>(),
            geometry
                .links_in_box(x, y, width, height)
                .into_iter()
                .collect::<std::collections::HashSet<_>>(),
        )
    };

    let node_flags: Vec<bool> = rows_of(nodes)
        .iter()
        .map(|node| node_hits.contains(&node.id) || (shift && node.selected))
        .collect();
    let link_flags: Vec<bool> = rows_of(links)
        .iter()
        .map(|link| link_hits.contains(&link.id) || (shift && link.selected))
        .collect();
    set_selection_flags(nodes, |node| &mut node.selected, &node_flags);
    set_selection_flags(links, |link| &mut link.selected, &link_flags);
}
