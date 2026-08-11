//! Slint presentation bridge for the desktop application.

use super::QpwgraphApp;
use pw_graph_core::{Direction, PortId};
use pw_graph_ui::{
    pair_ports, port_type_color, CanvasAction, GraphViewSnapshot, NodeView, PortGroupView,
};
use slint::{Color, Model, ModelRc, SharedString, Timer, TimerMode, VecModel};
use slint_node_editor::{GraphLogic, MovableNode, NodeEditorController, NodeEditorSetup};
use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

slint::include_modules!();

impl MovableNode for NodeData {
    fn id(&self) -> i32 {
        self.id
    }

    fn x(&self) -> f32 {
        self.x
    }

    fn y(&self) -> f32 {
        self.y
    }

    fn selected(&self) -> bool {
        self.selected
    }

    fn set_x(&mut self, x: f32) {
        self.x = x;
    }

    fn set_y(&mut self, y: f32) {
        self.y = y;
    }
}

enum UiEvent {
    Action(String),
    Connect(i32, i32),
    LinkDropped(i32, f32, f32),
    RelayRowAction(String),
    EffectParameterChanged(String, f32),
    DeleteSelection,
    CommitDragged(i32),
}

pub(crate) struct UiBridge {
    window: MainWindow,
    app: Rc<RefCell<QpwgraphApp>>,
    nodes: Rc<VecModel<NodeData>>,
    links: Rc<VecModel<LinkData>>,
    minimap_nodes: Rc<VecModel<MinimapNode>>,
    relay_rows: Rc<VecModel<RelayRowData>>,
    events: Rc<RefCell<Vec<UiEvent>>>,
    controller: Rc<NodeEditorController>,
}

impl UiBridge {
    pub(crate) fn new(app: Rc<RefCell<QpwgraphApp>>) -> Result<Self, slint::PlatformError> {
        let window = MainWindow::new()?;
        let nodes = Rc::new(VecModel::default());
        let links = Rc::new(VecModel::default());
        let minimap_nodes = Rc::new(VecModel::default());
        let relay_rows = Rc::new(VecModel::default());
        let events = Rc::new(RefCell::new(Vec::new()));

        window.set_nodes(ModelRc::from(nodes.clone()));
        window.set_links(ModelRc::from(links.clone()));
        window.set_minimap_nodes(ModelRc::from(minimap_nodes.clone()));
        window.set_relay_rows(ModelRc::from(relay_rows.clone()));

        let setup = NodeEditorSetup::new({
            let nodes = nodes.clone();
            let events = events.clone();
            move |dragged, delta_x, delta_y| {
                GraphLogic::commit_drag(&nodes, dragged, delta_x, delta_y);
                events.borrow_mut().push(UiEvent::CommitDragged(dragged));
            }
        });
        let controller_from_setup = setup.controller().clone();
        slint_node_editor::wire_node_editor!(window, setup);

        let bridge = Self {
            window,
            app,
            nodes,
            links,
            minimap_nodes,
            relay_rows,
            events,
            controller: controller_from_setup,
        };
        bridge.install_callbacks();
        bridge.sync_models();
        Ok(bridge)
    }

    fn install_callbacks(&self) {
        let events = self.events.clone();
        self.window.on_action(move |action| {
            events
                .borrow_mut()
                .push(UiEvent::Action(action.to_string()));
        });

        let app = self.app.clone();
        let nodes = self.nodes.clone();
        let links = self.links.clone();
        self.window.on_graph_node_selected(move |id, shift| {
            apply_node_selection(&app, &nodes, &links, id, shift);
        });

        let app = self.app.clone();
        let nodes = self.nodes.clone();
        let links = self.links.clone();
        self.window.on_graph_link_selected(move |id, shift| {
            apply_link_selection(&app, &nodes, &links, id, shift);
        });

        let app = self.app.clone();
        let nodes = self.nodes.clone();
        let links = self.links.clone();
        self.window.on_graph_selection_cleared(move || {
            clear_selection(&app, &nodes, &links);
        });

        let app = self.app.clone();
        let nodes = self.nodes.clone();
        let links = self.links.clone();
        let controller = self.controller.clone();
        self.window
            .on_graph_box_selected(move |x, y, width, height, shift| {
                apply_box_selection(
                    &app,
                    &nodes,
                    &links,
                    &controller,
                    x,
                    y,
                    width,
                    height,
                    shift,
                );
            });

        let events = self.events.clone();
        self.window.on_graph_link_requested(move |start, end| {
            events.borrow_mut().push(UiEvent::Connect(start, end));
        });

        let events = self.events.clone();
        self.window.on_graph_link_dropped(move |start, x, y| {
            events.borrow_mut().push(UiEvent::LinkDropped(start, x, y));
        });

        let events = self.events.clone();
        self.window.on_relay_row_action(move |address| {
            events
                .borrow_mut()
                .push(UiEvent::RelayRowAction(address.to_string()));
        });

        let events = self.events.clone();
        self.window.on_effect_parameter_changed(move |id, value| {
            events
                .borrow_mut()
                .push(UiEvent::EffectParameterChanged(id.to_string(), value));
        });

        let events = self.events.clone();
        self.window.on_graph_delete_selection(move || {
            events.borrow_mut().push(UiEvent::DeleteSelection);
        });

        let controller = self.controller.clone();
        self.window.on_graph_compute_pin_at(move |x, y| {
            controller.cache().borrow().find_pin_at(x, y, 10.0)
        });

        let controller = self.controller.clone();
        self.window.on_graph_compute_link_at(move |x, y| {
            controller.find_link_at_world(x, y, 8.0, 50.0, 20)
        });

        let controller = self.controller.clone();
        let weak_window = self.window.as_weak();
        self.window.on_graph_request_grid(move || {
            if let Some(window) = weak_window.upgrade() {
                window.set_grid_commands(controller.generate_grid(
                    window.get_width_(),
                    window.get_height_(),
                    window.get_pan_x(),
                    window.get_pan_y(),
                ));
            }
        });
    }

    pub(crate) fn run(self) -> Result<(), slint::PlatformError> {
        let timer = Timer::default();
        let app = self.app.clone();
        let window = self.window.as_weak();
        let nodes = self.nodes.clone();
        let links = self.links.clone();
        let minimap_nodes = self.minimap_nodes.clone();
        let relay_rows = self.relay_rows.clone();
        let events = self.events.clone();
        let controller = self.controller.clone();
        {
            let app = app.borrow();
            self.window.window().set_size(slint::PhysicalSize::new(
                app.config.window_width.max(320.0) as u32,
                app.config.window_height.max(240.0) as u32,
            ));
            if app.start_minimized {
                self.window.window().set_minimized(true);
            }
        }

        timer.start(TimerMode::Repeated, Duration::from_millis(50), move || {
            tick(
                &app,
                &window,
                &nodes,
                &links,
                &minimap_nodes,
                &relay_rows,
                &events,
                &controller,
            );
        });

        let result = self.window.run();
        timer.stop();
        finish_shutdown(&self.app);
        result
    }

    fn sync_models(&self) {
        let window = self.window.as_weak();
        sync_models(
            &self.app,
            &window,
            &self.nodes,
            &self.links,
            &self.minimap_nodes,
            &self.relay_rows,
            &self.controller,
        );
    }
}

fn tick(
    app: &Rc<RefCell<QpwgraphApp>>,
    window: &slint::Weak<MainWindow>,
    nodes: &Rc<VecModel<NodeData>>,
    links: &Rc<VecModel<LinkData>>,
    minimap_nodes: &Rc<VecModel<MinimapNode>>,
    relay_rows: &Rc<VecModel<RelayRowData>>,
    events: &Rc<RefCell<Vec<UiEvent>>>,
    controller: &Rc<NodeEditorController>,
) {
    read_window_state(app, window);
    let pending = std::mem::take(&mut *events.borrow_mut());
    for event in pending {
        process_event(app, nodes, links, event);
    }

    {
        let mut app = app.borrow_mut();
        app.sync_meter_policy();
        #[cfg(feature = "relay")]
        app.with_relay(|app, relay| relay.poll(app));

        if app.last_graph_refresh.elapsed() >= Duration::from_millis(100)
            && app.driver.graph_dirty()
        {
            match app.driver.refresh() {
                Ok(_) => app.last_graph_refresh = Instant::now(),
                Err(error) => app.status_error("status.refresh_failed", &error),
            }
        }
        if app.last_meter_refresh.elapsed() >= Duration::from_millis(50) {
            app.refresh_audio_meters();
            app.last_meter_refresh = Instant::now();
        }
        app.update_canvas_from_config();
        // Slint owns the live viewport and text input. Read it after applying
        // persisted settings so a scroll, zoom, or search edit is not
        // overwritten by the next timer tick.
        read_window_state_locked(&mut app, window);
        app.sync_effect_controls();
        let window_visible = window
            .upgrade()
            .is_some_and(|window| window.window().is_visible() && !window.window().is_minimized());
        app.request_visible_meters(window_visible);
        app.autosave_config();

        #[cfg(all(target_os = "linux", feature = "tray"))]
        poll_tray(&mut app, window);
    }

    sync_models(
        app,
        window,
        nodes,
        links,
        minimap_nodes,
        relay_rows,
        controller,
    );
}

fn process_event(
    app: &Rc<RefCell<QpwgraphApp>>,
    nodes: &Rc<VecModel<NodeData>>,
    links: &Rc<VecModel<LinkData>>,
    event: UiEvent,
) {
    let mut app = app.borrow_mut();
    match event {
        UiEvent::Action(action) => handle_action(&mut app, &action),
        UiEvent::Connect(start, end) => {
            let graph = app.driver.graph().clone();
            let snapshot = app.canvas.snapshot(&graph);
            let first = pin_group(&snapshot, start)
                .map(|group| group.ports.clone())
                .unwrap_or_else(|| app.canvas.ids.port_id(start).into_iter().collect());
            let second = pin_group(&snapshot, end)
                .map(|group| group.ports.clone())
                .unwrap_or_else(|| app.canvas.ids.port_id(end).into_iter().collect());
            let pairs = pair_groups(&graph, &first, &second);
            app.handle_canvas_actions(vec![if pairs.len() == 1 {
                CanvasAction::Connect {
                    output: pairs[0].0,
                    input: pairs[0].1,
                }
            } else {
                CanvasAction::ConnectMany { pairs }
            }]);
        }
        UiEvent::LinkDropped(start, x, y) => {
            let graph = app.driver.graph().clone();
            let Some(source_ports) = app
                .canvas
                .snapshot(&graph)
                .nodes
                .iter()
                .flat_map(|node| node.ports.iter())
                .find(|group| group.pin_id == start)
                .map(|group| group.ports.clone())
            else {
                return;
            };
            let target = graph.nodes.values().find(|node| {
                let height = 70.0 + node.ports.len() as f32 * 22.0;
                x >= node.position[0]
                    && y >= node.position[1]
                    && x <= node.position[0] + 280.0
                    && y <= node.position[1] + height
            });
            let Some(target) = target else { return };
            let inputs: Vec<_> = target
                .ports
                .iter()
                .filter_map(|id| graph.port(*id))
                .filter(|port| port.direction == Direction::Sink)
                .map(|port| port.id)
                .collect();
            let pairs = pair_groups(&graph, &source_ports, &inputs);
            if !pairs.is_empty() {
                app.handle_canvas_actions(vec![if pairs.len() == 1 {
                    CanvasAction::Connect {
                        output: pairs[0].0,
                        input: pairs[0].1,
                    }
                } else {
                    CanvasAction::ConnectMany { pairs }
                }]);
            }
        }
        UiEvent::RelayRowAction(address) => {
            #[cfg(feature = "relay")]
            app.with_relay(|app, relay| {
                let Some(row) = relay
                    .device_rows(app)
                    .into_iter()
                    .find(|row| row.addr.to_string() == address)
                else {
                    return;
                };
                match row.state {
                    super::relay::RelayDeviceState::Available => {
                        relay.connect_target(app, &address);
                    }
                    super::relay::RelayDeviceState::Connecting => relay.cancel_connect(),
                    super::relay::RelayDeviceState::Connected(session) => {
                        relay.disconnect(app, session)
                    }
                }
            });
        }
        UiEvent::EffectParameterChanged(id, value) => {
            if let Some(gallery) = app.effect_gallery.as_mut() {
                gallery.parameters.insert(id, value);
            }
        }
        UiEvent::DeleteSelection => {
            let selected_links = app.canvas.selected_links(app.driver.graph());
            let selected_nodes: Vec<_> = app.canvas.selected_nodes.iter().copied().collect();
            let mut actions = Vec::new();
            if !selected_links.is_empty() {
                actions.push(CanvasAction::DisconnectMany {
                    links: selected_links,
                });
            }
            actions.extend(
                selected_nodes
                    .into_iter()
                    .map(|node| CanvasAction::DisconnectNode { node }),
            );
            app.handle_canvas_actions(actions);
        }
        UiEvent::CommitDragged(dragged) => {
            commit_drag(&mut app, nodes, dragged);
        }
    }
    let _ = links;
}

fn handle_action(app: &mut QpwgraphApp, action: &str) {
    match action {
        "refresh" => app.refresh_graph(),
        "arrange" => app.arrange_nodes(),
        "pan-left" => app.canvas.pan[0] -= 48.0,
        "pan-right" => app.canvas.pan[0] += 48.0,
        "pan-up" => app.canvas.pan[1] -= 48.0,
        "pan-down" => app.canvas.pan[1] += 48.0,
        "pan-left-medium" => app.canvas.pan[0] -= 96.0,
        "pan-right-medium" => app.canvas.pan[0] += 96.0,
        "pan-up-medium" => app.canvas.pan[1] -= 96.0,
        "pan-down-medium" => app.canvas.pan[1] += 96.0,
        "pan-left-fast" => app.canvas.pan[0] -= 192.0,
        "pan-right-fast" => app.canvas.pan[0] += 192.0,
        "pan-up-fast" => app.canvas.pan[1] -= 192.0,
        "pan-down-fast" => app.canvas.pan[1] += 192.0,
        "zoom-in" => app.canvas.zoom = (app.canvas.zoom * 1.1).clamp(0.35, 2.5),
        "zoom-out" => app.canvas.zoom = (app.canvas.zoom / 1.1).clamp(0.35, 2.5),
        "toggle-thumbnail" => app.canvas.thumbnail_mode = !app.canvas.thumbnail_mode,
        "toggle-minimap" => app.canvas.minimap_visible = !app.canvas.minimap_visible,
        "undo" => app.undo(),
        "redo" => app.redo(),
        "save-config" => app.save_config_now(),
        "save-patchbay" => app.save_patchbay(),
        "load-patchbay" => app.load_patchbay(),
        "disconnect-all" => app.disconnect_all(),
        "snapshot-patchbay" => app.snapshot_patchbay(),
        "activate-patchbay" => app.activate_patchbay(),
        "reset-audio" => app.reset_audio_config(),
        "toggle-statusbar" => app.config.statusbar = !app.config.statusbar,
        "toggle-repel" => {
            app.config.repel_overlapping_nodes = !app.config.repel_overlapping_nodes;
            app.canvas.repel_overlapping_nodes = app.config.repel_overlapping_nodes;
        }
        "toggle-connect-through" => {
            app.config.connect_through_nodes = !app.config.connect_through_nodes;
            app.canvas.connect_through_nodes = app.config.connect_through_nodes;
        }
        "sort-name" => app.config.sort_type = "name".into(),
        "sort-id" => app.config.sort_type = "id".into(),
        "sort-ascending" => app.config.sort_order = "ascending".into(),
        "sort-descending" => app.config.sort_order = "descending".into(),
        "connect-easy" => {
            app.canvas.connect_mode = pw_graph_ui::ConnectMode::Easy;
            app.config.connect_mode = "easy".into();
        }
        "connect-advanced" => {
            app.canvas.connect_mode = pw_graph_ui::ConnectMode::Advanced;
            app.config.connect_mode = "advanced".into();
        }
        "filter-audio" => app.canvas.media_filter = pw_graph_ui::MediaFilter::Audio,
        "filter-video" => app.canvas.media_filter = pw_graph_ui::MediaFilter::Video,
        "filter-midi" => app.canvas.media_filter = pw_graph_ui::MediaFilter::Midi,
        "filter-all" => app.canvas.media_filter = pw_graph_ui::MediaFilter::All,
        "preferences" => {
            app.show_preferences = !app.show_preferences;
            app.show_history = false;
            app.show_shortcuts = false;
            app.show_effects = false;
        }
        "history" => {
            app.show_history = !app.show_history;
            app.show_preferences = false;
            app.show_shortcuts = false;
            app.show_effects = false;
        }
        "shortcuts" => {
            app.show_shortcuts = !app.show_shortcuts;
            app.show_preferences = false;
            app.show_history = false;
            app.show_effects = false;
        }
        "effects" => {
            app.show_effects = !app.show_effects;
            app.show_preferences = false;
            app.show_history = false;
            app.show_shortcuts = false;
            if app.show_effects {
                app.open_effect_gallery();
            }
        }
        "create-effect" => {
            if let Some(gallery) = app.effect_gallery.clone() {
                if app.create_effect_from_gallery(&gallery) {
                    app.show_effects = false;
                }
            } else {
                app.open_effect_gallery();
            }
        }
        "effect-configure" => {
            if let Some(gallery) = app.effect_gallery.as_mut() {
                gallery.next_phase();
            }
        }
        "effect-choose" => {
            if let Some(gallery) = app.effect_gallery.as_mut() {
                gallery.previous_phase();
            }
        }
        "effect-toggle-enabled" => {
            if let Some(gallery) = app.effect_gallery.as_mut() {
                gallery.enabled = !gallery.enabled;
            }
        }
        "relay" => {
            #[cfg(feature = "relay")]
            {
                app.show_relay = !app.show_relay;
                if app.show_relay {
                    app.with_relay(|app, relay| relay.start_discovery(app));
                }
            }
        }
        "relay-discover" => {
            #[cfg(feature = "relay")]
            app.with_relay(|app, relay| relay.start_discovery(app));
        }
        "relay-refresh" => {
            #[cfg(feature = "relay")]
            app.with_relay(|app, relay| relay.refresh(app));
        }
        "relay-connect" => {
            #[cfg(feature = "relay")]
            {
                let target = app.relay.quick_target.clone();
                app.config.relay_client_target = target;
                app.with_relay(|app, relay| relay.connect(app));
            }
        }
        "relay-host-start" => {
            #[cfg(feature = "relay")]
            app.with_relay(|app, relay| relay.start_host(app));
        }
        "relay-host-stop" => {
            #[cfg(feature = "relay")]
            app.with_relay(|app, relay| relay.stop_host(app));
        }
        "relay-show-qr" => {
            #[cfg(feature = "relay")]
            {
                app.with_relay(|app, relay| {
                    relay.show_qr = true;
                    relay.qr_text = super::relay::RelayUiState::qr_payload(app)
                        .unwrap_or_else(|| app.t("relay.no_links"));
                    relay.message = relay.qr_text.clone();
                    app.status = relay.qr_text.clone();
                });
            }
        }
        "escape" => {
            app.show_shortcuts = false;
            app.show_history = false;
            app.show_preferences = false;
            app.show_effects = false;
            #[cfg(feature = "relay")]
            {
                app.show_relay = false;
            }
        }
        _ => {}
    }
}

fn sync_models(
    app: &Rc<RefCell<QpwgraphApp>>,
    window: &slint::Weak<MainWindow>,
    nodes: &Rc<VecModel<NodeData>>,
    links: &Rc<VecModel<LinkData>>,
    minimap_nodes: &Rc<VecModel<MinimapNode>>,
    relay_rows: &Rc<VecModel<RelayRowData>>,
    controller: &Rc<NodeEditorController>,
) {
    let Some(window) = window.upgrade() else {
        return;
    };
    let mut app = app.borrow_mut();
    let graph = app.driver.graph().clone();
    let snapshot = app.canvas.snapshot(&graph);
    // The node-editor mutates the Slint rows during a drag and commits once
    // on release. Replacing those rows from the backend in the middle of the
    // gesture would visibly snap the node back before the command is queued.
    if controller.dragged_node_id() == 0 {
        let node_rows: Vec<_> = snapshot.nodes.iter().map(node_data).collect();
        nodes.set_vec(node_rows);
        minimap_nodes.set_vec(
            snapshot
                .nodes
                .iter()
                .map(|node| MinimapNode {
                    id: node.id,
                    x: node.position[0],
                    y: node.position[1],
                    width: 280.0,
                    height: 100.0 + node.ports.len() as f32 * 22.0,
                    color: color(node.appearance.color.unwrap_or([63, 82, 101, 255])),
                })
                .collect::<Vec<_>>(),
        );
    }
    let link_rows: Vec<_> = snapshot.links.iter().map(link_data).collect();
    links.set_vec(link_rows);
    #[cfg(feature = "relay")]
    {
        let rows = app.relay.device_rows(&app);
        relay_rows.set_vec(
            rows.iter()
                .map(|row| relay_row_data(row, &app.relay.levels))
                .collect::<Vec<_>>(),
        );
    }
    #[cfg(not(feature = "relay"))]
    relay_rows.set_vec(Vec::new());
    controller.clear_links();
    for link in &snapshot.links {
        controller.register_link(link.id, link.start_pin_id, link.end_pin_id);
    }
    window.set_status(SharedString::from(app.status.clone()));
    window.set_backend(SharedString::from(app.backend_name.clone()));
    let (node_count, port_count, link_count) = app.canvas.visible_counts(&graph);
    window.set_graph_counts(SharedString::from(format!(
        "{node_count} nodes · {port_count} ports · {link_count} links"
    )));
    window.set_show_statusbar(app.config.statusbar);
    window.set_show_minimap(app.canvas.minimap_visible);
    window.set_show_preferences(app.show_preferences);
    window.set_show_history(app.show_history);
    window.set_show_shortcuts(app.show_shortcuts);
    window.set_show_effects(app.show_effects);
    window.set_repel_overlapping(app.canvas.repel_overlapping_nodes);
    window.set_connect_through(app.canvas.connect_through_nodes);
    let effect_parameters = app
        .effect_gallery
        .as_ref()
        .and_then(|gallery| {
            super::effects::available_descriptors(app.driver.as_ref())
                .into_iter()
                .find(|descriptor| descriptor.id == gallery.effect_id)
                .map(|descriptor| {
                    descriptor
                        .parameters
                        .iter()
                        .map(|parameter| EffectParameterData {
                            id: SharedString::from(parameter.id.clone()),
                            name: SharedString::from(parameter.name.clone()),
                            minimum: parameter.minimum,
                            maximum: parameter.maximum,
                            value: gallery
                                .parameters
                                .get(&parameter.id)
                                .copied()
                                .unwrap_or(parameter.default),
                            unit: SharedString::from(parameter.unit.clone()),
                            boolean: parameter.unit == "boolean",
                        })
                        .collect::<Vec<_>>()
                })
        })
        .unwrap_or_default();
    window.set_effect_parameters(ModelRc::from(Rc::new(VecModel::from(effect_parameters))));
    window.set_effect_enabled(
        app.effect_gallery
            .as_ref()
            .is_none_or(|gallery| gallery.enabled),
    );
    let effect_message = app
        .effect_gallery
        .as_ref()
        .map(|gallery| {
            let _scroll_epoch = gallery.scroll_epoch;
            format!(
                "{} · {} · step {}/2",
                app.status,
                gallery.effect_id,
                gallery.phase.index() + 1
            )
        })
        .unwrap_or_else(|| app.status.clone());
    window.set_effect_message(SharedString::from(effect_message));
    #[cfg(feature = "relay")]
    {
        window.set_show_relay(app.show_relay);
        window.set_relay_target(SharedString::from(app.relay.quick_target.clone()));
        window.set_relay_message(SharedString::from(app.relay.message.clone()));
    }
    window.set_connect_mode(SharedString::from(app.canvas.connect_mode.as_str()));
    window.set_media_filter(SharedString::from(app.canvas.media_filter.as_str()));
    window.set_search_text(SharedString::from(app.canvas.search_query.clone()));
    window.set_pan_x(app.canvas.pan[0]);
    window.set_pan_y(app.canvas.pan[1]);
    window.set_zoom(app.canvas.zoom);
}

#[cfg(feature = "relay")]
fn relay_row_data(
    row: &super::relay::RelayDeviceRow,
    levels: &std::collections::HashMap<u64, f32>,
) -> RelayRowData {
    let (state, level) = match row.state {
        super::relay::RelayDeviceState::Available => ("available", 0.0),
        super::relay::RelayDeviceState::Connecting => ("connecting", 0.0),
        super::relay::RelayDeviceState::Connected(session) => (
            "connected",
            levels
                .get(&session.0)
                .copied()
                .unwrap_or(0.0)
                .clamp(0.0, 1.0),
        ),
    };
    RelayRowData {
        name: SharedString::from(row.name.clone()),
        address: SharedString::from(row.addr.to_string()),
        state: SharedString::from(state),
        level,
    }
}

fn read_window_state(app: &Rc<RefCell<QpwgraphApp>>, window: &slint::Weak<MainWindow>) {
    let mut app = app.borrow_mut();
    read_window_state_locked(&mut app, window);
}

fn read_window_state_locked(app: &mut QpwgraphApp, window: &slint::Weak<MainWindow>) {
    let Some(window) = window.upgrade() else {
        return;
    };
    app.canvas.pan = [window.get_pan_x(), window.get_pan_y()];
    app.canvas.zoom = window.get_zoom().clamp(0.35, 2.5);
    app.canvas.search_query = window.get_search_text().to_string();
    app.update_window_size(window.get_width_(), window.get_height_());
    #[cfg(feature = "relay")]
    {
        app.relay.quick_target = window.get_relay_target().to_string();
    }
}

fn node_data(node: &NodeView) -> NodeData {
    let ports: Vec<_> = node
        .ports
        .iter()
        .enumerate()
        .map(|(index, port)| PortData {
            id: port.pin_id,
            label: SharedString::from(port.label.clone()),
            direction: if port.direction == Direction::Sink {
                0
            } else {
                1
            },
            color: color(port_type_color(port.port_type)),
            y: index as f32 * 22.0,
        })
        .collect();
    NodeData {
        id: node.id,
        title: SharedString::from(node.title.clone()),
        node_type: SharedString::from(format!("{:?}", node.node_type)),
        x: node.position[0],
        y: node.position[1],
        width: 280.0,
        height: 70.0 + ports.len() as f32 * 22.0,
        selected: node.selected,
        collapsed: node.appearance.collapsed,
        color: color(node.appearance.color.unwrap_or([63, 82, 101, 255])),
        ports: ModelRc::from(Rc::new(VecModel::from(ports))),
    }
}

fn link_data(link: &pw_graph_ui::LinkView) -> LinkData {
    LinkData {
        id: link.id,
        start_pin_id: link.start_pin_id,
        end_pin_id: link.end_pin_id,
        color: color(link.color),
        selected: link.selected,
        line_width: 2.0,
        status: -1,
    }
}

fn color(rgba: [u8; 4]) -> Color {
    Color::from_argb_u8(rgba[3], rgba[0], rgba[1], rgba[2])
}

fn pin_group<'a>(snapshot: &'a GraphViewSnapshot, pin: i32) -> Option<&'a PortGroupView> {
    snapshot
        .nodes
        .iter()
        .flat_map(|node| node.ports.iter())
        .find(|group| group.pin_id == pin)
}

fn pair_groups(
    graph: &pw_graph_core::Graph,
    first: &[PortId],
    second: &[PortId],
) -> Vec<(PortId, PortId)> {
    pair_ports(graph, first, second)
}

fn apply_node_selection(
    app: &Rc<RefCell<QpwgraphApp>>,
    nodes: &Rc<VecModel<NodeData>>,
    links: &Rc<VecModel<LinkData>>,
    id: i32,
    shift: bool,
) {
    for index in 0..nodes.row_count() {
        let Some(mut row) = nodes.row_data(index) else {
            continue;
        };
        if row.id == id {
            row.selected = if shift { !row.selected } else { true };
        } else if !shift {
            row.selected = false;
        }
        nodes.set_row_data(index, row);
    }
    if !shift {
        clear_link_flags(links);
    }
    sync_selection_state(app, nodes, links);
}

fn apply_link_selection(
    app: &Rc<RefCell<QpwgraphApp>>,
    nodes: &Rc<VecModel<NodeData>>,
    links: &Rc<VecModel<LinkData>>,
    id: i32,
    shift: bool,
) {
    if !shift {
        clear_node_flags(nodes);
    }
    for index in 0..links.row_count() {
        let Some(mut row) = links.row_data(index) else {
            continue;
        };
        if row.id == id {
            row.selected = if shift { !row.selected } else { true };
        } else if !shift {
            row.selected = false;
        }
        links.set_row_data(index, row);
    }
    sync_selection_state(app, nodes, links);
}

fn clear_selection(
    app: &Rc<RefCell<QpwgraphApp>>,
    nodes: &Rc<VecModel<NodeData>>,
    links: &Rc<VecModel<LinkData>>,
) {
    clear_node_flags(nodes);
    clear_link_flags(links);
    sync_selection_state(app, nodes, links);
}

fn apply_box_selection(
    app: &Rc<RefCell<QpwgraphApp>>,
    nodes: &Rc<VecModel<NodeData>>,
    links: &Rc<VecModel<LinkData>>,
    controller: &Rc<NodeEditorController>,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    shift: bool,
) {
    let hits = controller
        .cache()
        .borrow()
        .nodes_in_selection_box(x, y, width, height);
    for index in 0..nodes.row_count() {
        let Some(mut row) = nodes.row_data(index) else {
            continue;
        };
        let hit = hits.contains(&row.id);
        row.selected = if shift { row.selected || hit } else { hit };
        nodes.set_row_data(index, row);
    }
    if !shift {
        clear_link_flags(links);
    }
    sync_selection_state(app, nodes, links);
}

fn clear_node_flags(nodes: &Rc<VecModel<NodeData>>) {
    for index in 0..nodes.row_count() {
        if let Some(mut row) = nodes.row_data(index) {
            row.selected = false;
            nodes.set_row_data(index, row);
        }
    }
}

fn clear_link_flags(links: &Rc<VecModel<LinkData>>) {
    for index in 0..links.row_count() {
        if let Some(mut row) = links.row_data(index) {
            row.selected = false;
            links.set_row_data(index, row);
        }
    }
}

fn sync_selection_state(
    app: &Rc<RefCell<QpwgraphApp>>,
    nodes: &Rc<VecModel<NodeData>>,
    links: &Rc<VecModel<LinkData>>,
) {
    let mut app = app.borrow_mut();
    app.canvas.selected_nodes = (0..nodes.row_count())
        .filter_map(|index| nodes.row_data(index))
        .filter(|row| row.selected)
        .filter_map(|row| app.canvas.ids.node_id(row.id))
        .collect();
    app.canvas.selected_node = app.canvas.selected_nodes.iter().next().copied();
    app.canvas.selected_link = (0..links.row_count())
        .filter_map(|index| links.row_data(index))
        .find(|row| row.selected)
        .and_then(|row| app.canvas.ids.link_id(row.id));
}

fn commit_drag(app: &mut QpwgraphApp, nodes: &Rc<VecModel<NodeData>>, dragged: i32) {
    let Some(dragged_id) = app.canvas.ids.node_id(dragged) else {
        return;
    };
    let mut before = Vec::new();
    let mut after = Vec::new();
    for index in 0..nodes.row_count() {
        let Some(row) = nodes.row_data(index) else {
            continue;
        };
        if !row.selected && row.id != dragged {
            continue;
        }
        let Some(node_id) = app.canvas.ids.node_id(row.id) else {
            continue;
        };
        let Some(node) = app.driver.graph().node(node_id) else {
            continue;
        };
        before.push((node_id, node.position));
        after.push((node_id, [row.x, row.y]));
    }
    if before != after {
        app.handle_canvas_actions(vec![CanvasAction::CommitNodeMove { before, after }]);
    } else if let Some(row) = (0..nodes.row_count())
        .filter_map(|index| nodes.row_data(index))
        .find(|row| row.id == dragged)
    {
        let previous = app
            .driver
            .graph()
            .node(dragged_id)
            .map(|node| node.position);
        if let Some(previous) = previous {
            let _ = app.driver.set_node_position(dragged_id, [row.x, row.y]);
            if previous != [row.x, row.y] {
                app.status = app.t("status.node_moved");
            }
        }
    }
}

fn finish_shutdown(app: &Rc<RefCell<QpwgraphApp>>) {
    let mut app = app.borrow_mut();
    #[cfg(all(target_os = "linux", feature = "tray"))]
    if let Some(tray) = app.tray.as_ref() {
        tray.shutdown();
    }
    #[cfg(feature = "relay")]
    app.driver.relay_discovery_stop();
    app.sync_config();
    app.sync_patchbay_connections();
    app.autosave_patchbay();
    if let Err(error) = app.config.save_to(&app.config_file) {
        eprintln!(
            "{}",
            app.tf("status.config_save_failed", &[("error", error.to_string())])
        );
    }
}

#[cfg(all(target_os = "linux", feature = "tray"))]
fn poll_tray(app: &mut QpwgraphApp, window: &slint::Weak<MainWindow>) {
    let Some(tray) = app.tray.as_ref() else {
        return;
    };
    while let Ok(command) = tray.receiver.try_recv() {
        let Some(window) = window.upgrade() else {
            return;
        };
        match command {
            crate::tray::tray_support::Command::Show => {
                let _ = window.window().show();
                window.window().set_minimized(false);
                app.start_minimized = false;
            }
            crate::tray::tray_support::Command::Hide => {
                let _ = window.window().hide();
                app.start_minimized = true;
            }
            crate::tray::tray_support::Command::Quit => {
                let _ = window.window().hide();
                let _ = slint::quit_event_loop();
            }
        }
    }
}
