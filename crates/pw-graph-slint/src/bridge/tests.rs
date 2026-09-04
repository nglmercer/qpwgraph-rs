use super::actions::handle_action;
use super::connections::{
    connect_pin_pair, delete_selected_connections, easy_connect_from_pin, easy_connect_nodes,
    handle_link_requested,
};
use super::*;
use crate::canvas::{self, HIT_NODE, HIT_NODE_BODY};
use crate::model::{resolve_drag_delta, ConnectMode};
use pw_graph_core::Direction;
use slint::platform::{PointerEventButton, WindowEvent};
use slint::{LogicalPosition, ModelRc};
use std::collections::BTreeSet;
use std::path::PathBuf;

pub(super) fn demo_application() -> Application {
    let args = Args {
        demo: true,
        ..Args::default()
    };
    let i18n = I18n::from_language("en");
    let (source, status) = ApplicationDriver::new(&args, MeterPolicy::Disabled, &i18n);
    let config = AppConfig::default();
    let mut view = UiGraphState::from_config(&config);
    let snapshot = view.snapshot(source.graph(), &config);
    Application {
        source,
        commands: pw_graph_command::CommandStack::new(),
        patchbay: Patchbay::new("test"),
        patchbay_file: PathBuf::new(),
        config: config.clone(),
        config_file: PathBuf::new(),
        config_saved_snapshot: config,
        config_dirty_since: None,
        i18n,
        view,
        snapshot,
        status,
        toast_message: String::new(),
        toast_until: None,
        toast_error: false,
        pending_connection_pin: None,
        effect_draft_id: None,
        effect_draft_enabled: true,
        effect_draft_parameters: BTreeMap::new(),
        debug: false,
        last_refresh: Instant::now(),
        meters: BTreeMap::new(),
        meter_error: None,
        #[cfg(feature = "relay")]
        relay_levels: BTreeMap::new(),
        #[cfg(feature = "relay")]
        relay_connecting: None,
        #[cfg(feature = "relay")]
        relay_discovery_active: false,
        #[cfg(feature = "relay")]
        relay_usb_present: false,
        #[cfg(feature = "relay")]
        relay_usb_last_poll: None,
        #[cfg(feature = "relay")]
        relay_usb_auto_attempted: false,
        #[cfg(feature = "relay")]
        relay_trusted_auto_attempt_at: None,
        #[cfg(feature = "relay")]
        relay_trusted_candidate_failures: BTreeMap::new(),
        #[cfg(feature = "relay")]
        relay_trusted_refused: BTreeSet::new(),
        #[cfg(feature = "relay")]
        relay_pending_enrollment: None,
        #[cfg(feature = "relay")]
        relay_reconnect_pending: None,
        #[cfg(feature = "relay")]
        relay_direction_switch: None,
        #[cfg(feature = "relay")]
        relay_direction_ui_sync: None,
    }
}

#[test]
fn advanced_pin_connections_reach_demo_backend_in_both_directions() {
    let mut application = demo_application();
    let output = application.view.ids.port(pw_graph_core::PortId(1)).unwrap();
    let input = application.view.ids.port(pw_graph_core::PortId(3)).unwrap();

    connect_pin_pair(&mut application, output, input);
    assert!(application
        .source
        .graph()
        .links
        .values()
        .any(|link| link.output_port.0 == 1 && link.input_port.0 == 3));
    assert_eq!(application.toast_message, "Connection created");
    assert!(!application.toast_error);

    connect_pin_pair(&mut application, input, output);
    assert_eq!(application.toast_message, "Connection already exists");
    assert!(!application.toast_error);
}

#[test]
fn advanced_connection_rejects_stale_and_same_direction_pins() {
    let mut application = demo_application();
    let output = application.view.ids.port(pw_graph_core::PortId(1)).unwrap();
    let other_output = application.view.ids.port(pw_graph_core::PortId(2)).unwrap();

    handle_link_requested(&mut application, output, output);
    assert_eq!(application.pending_connection_pin, Some(output));
    assert!(!application.toast_error);
    assert!(application
        .toast_message
        .contains("click a destination pin"));

    handle_link_requested(&mut application, output, output);
    assert_eq!(application.pending_connection_pin, None);
    assert_eq!(application.toast_message, "Connection cancelled");

    connect_pin_pair(&mut application, output, 99_999);
    assert!(application.toast_error);
    assert!(application.toast_message.contains("no longer available"));

    connect_pin_pair(&mut application, output, other_output);
    assert!(application.toast_error);
    assert!(application.toast_message.contains("one output pin"));
}

#[test]
fn delete_removes_the_selected_connection() {
    let mut application = demo_application();
    let output = application.view.ids.port(pw_graph_core::PortId(1)).unwrap();
    let input = application.view.ids.port(pw_graph_core::PortId(3)).unwrap();
    connect_pin_pair(&mut application, output, input);
    let link = *application.source.graph().links.keys().next().unwrap();
    application.view.selected_links.insert(link);

    delete_selected_connections(&mut application);

    assert!(application.source.graph().links.is_empty());
    assert!(application.view.selected_links.is_empty());
    assert_eq!(application.toast_message, "Removed 1 connection(s)");
    assert!(!application.toast_error);
}

#[test]
fn easy_connections_create_all_matching_demo_channels() {
    let mut application = demo_application();
    let source = application.view.ids.node(pw_graph_core::NodeId(1)).unwrap();
    let target_position = application
        .snapshot
        .nodes
        .iter()
        .find(|node| node.node_id == pw_graph_core::NodeId(2))
        .map(|node| node.position)
        .unwrap();

    easy_connect_nodes(
        &mut application,
        source,
        target_position[0] + 10.0,
        target_position[1] + 10.0,
        0,
    );

    assert_eq!(application.source.graph().links.len(), 2);
    assert!(application.toast_message.contains("created 2 connection"));
    assert!(channels_are_paired_straight(&application));
}

#[test]
fn easy_drop_accepts_the_visible_pin_margin() {
    let mut application = demo_application();
    let source = application.view.ids.node(pw_graph_core::NodeId(1)).unwrap();
    let target = application
        .snapshot
        .nodes
        .iter()
        .find(|node| node.node_id == pw_graph_core::NodeId(2))
        .unwrap();
    let pin_edge_x = target.position[0] - 6.0;
    let pin_y = target.position[1] + target.height / 2.0;

    easy_connect_nodes(&mut application, source, pin_edge_x, pin_y, 0);

    assert_eq!(application.source.graph().links.len(), 2);
    assert!(application.toast_message.contains("created 2 connection"));
}

#[test]
fn easy_port_drag_connects_when_released_on_the_target_body() {
    let mut application = demo_application();
    // This drop is only reachable in Easy mode, where the pin the drag
    // started on stands for the whole capture channel group.
    application.view.connect_mode = ConnectMode::Easy;
    let source_pin = application.view.ids.port(pw_graph_core::PortId(1)).unwrap();
    let (target_x, target_y) = application
        .snapshot
        .nodes
        .iter()
        .find(|node| node.node_id == pw_graph_core::NodeId(2))
        .map(|node| {
            (
                node.position[0] + node.width / 2.0,
                node.position[1] + node.height / 2.0,
            )
        })
        .unwrap();

    easy_connect_from_pin(&mut application, source_pin, target_x, target_y);

    assert_eq!(application.source.graph().links.len(), 2);
    assert!(application.toast_message.contains("created 2 connection"));
    assert!(!application.toast_error);
    assert!(channels_are_paired_straight(&application));
}

#[test]
fn two_pin_clicks_connect_without_holding_the_pointer() {
    let mut application = demo_application();
    let output = application.view.ids.port(pw_graph_core::PortId(1)).unwrap();
    let input = application.view.ids.port(pw_graph_core::PortId(3)).unwrap();

    handle_link_requested(&mut application, output, output);
    assert_eq!(application.pending_connection_pin, Some(output));

    handle_link_requested(&mut application, input, input);

    assert_eq!(application.pending_connection_pin, None);
    assert_eq!(application.source.graph().links.len(), 1);
    assert_eq!(application.toast_message, "Connection created");
    assert!(!application.toast_error);
}

#[test]
fn two_pin_clicks_group_channels_in_easy_mode() {
    let mut application = demo_application();
    application.view.connect_mode = ConnectMode::Easy;
    let output = application.view.ids.port(pw_graph_core::PortId(1)).unwrap();
    let input = application.view.ids.port(pw_graph_core::PortId(3)).unwrap();

    handle_link_requested(&mut application, output, output);
    handle_link_requested(&mut application, input, input);

    assert_eq!(application.pending_connection_pin, None);
    assert_eq!(application.source.graph().links.len(), 2);
    assert!(application.toast_message.contains("created 2 connection"));
    assert!(!application.toast_error);
    assert!(channels_are_paired_straight(&application));
}

#[test]
fn connection_feedback_is_transient() {
    let mut application = demo_application();
    set_connection_feedback(&mut application, "test connection", false);
    assert!(toast_visible(&application));

    application.toast_until = Some(Instant::now() - Duration::from_secs(1));
    assert!(!toast_visible(&application));
}

#[test]
fn shortcut_catalog_matches_the_documented_help_dialog() {
    let i18n = I18n::from_language("en");
    assert_eq!(shortcut_rows(&i18n, "").len(), 22);
    let filtered = shortcut_rows(&i18n, "thumbnail");
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].keys.as_str(), "T");
}

#[test]
fn node_volume_track_preserves_unity_and_boost_range() {
    assert!((volume_from_track_position(0.9, 1.5) - 1.0).abs() < f32::EPSILON);
    assert!((volume_from_track_position(1.0, 1.5) - 1.5).abs() < f32::EPSILON);
    assert!((volume_from_track_position(0.45, 1.5) - 0.5).abs() < f32::EPSILON);
}

/// A backend clamped at unity must not offer boost travel. The Windows
/// endpoints clamp at 1.0, so the whole track has to map into 0..=1 --
/// otherwise the top tenth of the fader silently does nothing and the card
/// claims a level Core Audio discarded.
#[test]
fn a_node_clamped_at_unity_uses_the_whole_track_for_zero_to_one() {
    assert!((volume_from_track_position(1.0, 1.0) - 1.0).abs() < f32::EPSILON);
    assert!((volume_from_track_position(0.5, 1.0) - 0.5).abs() < f32::EPSILON);
    assert!((volume_from_track_position(0.0, 1.0)).abs() < f32::EPSILON);
}

#[test]
fn track_position_round_trips_through_volume_for_either_ceiling() {
    for max_volume in [1.0, 1.5] {
        for step in 0..=20 {
            let position = step as f32 / 20.0;
            let volume = volume_from_track_position(position, max_volume);
            let back = track_position_from_volume(volume, max_volume);
            assert!(
                (back - position).abs() < 0.001,
                "max {max_volume} position {position} came back as {back}"
            );
            assert!(volume <= max_volume + f32::EPSILON);
        }
    }
}

#[test]
fn meter_track_uses_dbfs_scale() {
    assert_eq!(meter_fraction(0.0), 0.0);
    assert_eq!(meter_fraction(0.001), 0.0);
    assert!((meter_fraction(0.01) - (1.0 / 3.0)).abs() < 0.001);
    assert_eq!(meter_fraction(1.0), 1.0);
}

#[test]
fn audio_slider_updates_are_coalesced_per_node() {
    let compacted = coalesce_audio_volume_events(vec![
        UiEvent::SetAudioVolume(7, 0.1),
        UiEvent::SetAudioVolume(7, 0.8),
        UiEvent::SetAudioVolume(8, 0.4),
    ]);
    assert_eq!(compacted.len(), 2);
    assert!(
        matches!(compacted[0], UiEvent::SetAudioVolume(7, value) if (value - 0.8).abs() < f32::EPSILON)
    );
    assert!(
        matches!(compacted[1], UiEvent::SetAudioVolume(8, value) if (value - 0.4).abs() < f32::EPSILON)
    );
}

#[test]
fn preference_indices_round_trip_supported_values() {
    for policy in MeterPolicy::ALL {
        assert_eq!(meter_policy_from_index(meter_policy_index(policy)), policy);
    }
    for code in ["en", "es", "fr"] {
        assert_eq!(language_code(language_index(code)), code);
    }
}

/// Drives the real window with the real canvas wiring: rows, geometry and
/// callbacks are produced by the same code the application runs.
struct CanvasHarness {
    window: MainWindow,
    nodes: Rc<VecModel<NodeRow>>,
    links: Rc<VecModel<LinkRow>>,
    geometry: Rc<RefCell<CanvasGeometry>>,
    events: Rc<RefCell<Vec<UiEvent>>>,
    application: Application,
}

/// The graph starts at the right edge of the icon rail.
const RAIL_WIDTH: f32 = 76.0;

impl CanvasHarness {
    fn new(connect_mode: ConnectMode) -> Self {
        i_slint_backend_testing::init_no_event_loop();
        let mut application = demo_application();
        application.view.connect_mode = connect_mode;
        // Anchor the viewport so world and screen differ only by the rail.
        application.view.pan = [0.0, 0.0];
        application.view.zoom = 1.0;

        let window = MainWindow::new().unwrap();
        window
            .window()
            .set_size(slint::LogicalSize::new(1400.0, 900.0));
        let nodes = Rc::new(VecModel::default());
        let links = Rc::new(VecModel::default());
        window.set_nodes(ModelRc::from(nodes.clone()));
        window.set_links(ModelRc::from(links.clone()));
        let geometry = Rc::new(RefCell::new(CanvasGeometry::default()));
        let events = Rc::new(RefCell::new(Vec::new()));
        install_canvas_callbacks(&window, &nodes, &links, &geometry, &events);

        let mut harness = Self {
            window,
            nodes,
            links,
            geometry,
            events,
            application,
        };
        harness.sync();
        harness
    }

    fn sync(&mut self) {
        let minimap = Rc::new(VecModel::default());
        let version = Rc::new(Cell::new(0));
        sync_models(
            &self.window,
            &mut self.application,
            &self.nodes,
            &self.links,
            &minimap,
            &self.geometry,
            &version,
        );
    }

    fn screen_of(&self, world: (f32, f32)) -> LogicalPosition {
        let zoom = self.application.view.zoom;
        let pan = self.application.view.pan;
        LogicalPosition::new(
            RAIL_WIDTH + pan[0] + world.0 * zoom,
            pan[1] + world.1 * zoom,
        )
    }

    /// Collapse a card through the same event the card's chevron sends.
    fn collapse(&mut self, node_id: i32) {
        process_event(
            &self.window,
            &mut self.application,
            UiEvent::ToggleCollapse(node_id),
        );
        self.sync();
    }

    /// Zoom the viewport the way the toolbar does, then push it to the UI.
    fn set_zoom(&mut self, zoom: f32) {
        self.application.view.zoom = zoom;
        self.sync();
    }

    fn dispatch(&self, event: WindowEvent) {
        self.window.window().dispatch_event(event);
        slint::platform::update_timers_and_animations();
    }

    fn drag(&self, from: (f32, f32), to: (f32, f32)) {
        self.dispatch(WindowEvent::PointerPressed {
            position: self.screen_of(from),
            button: PointerEventButton::Left,
        });
        self.dispatch(WindowEvent::PointerMoved {
            position: self.screen_of(to),
        });
        self.dispatch(WindowEvent::PointerReleased {
            position: self.screen_of(to),
            button: PointerEventButton::Left,
        });
    }

    fn click(&self, at: (f32, f32)) {
        self.dispatch(WindowEvent::PointerPressed {
            position: self.screen_of(at),
            button: PointerEventButton::Left,
        });
        self.dispatch(WindowEvent::PointerReleased {
            position: self.screen_of(at),
            button: PointerEventButton::Left,
        });
    }

    fn take_events(&self) -> Vec<UiEvent> {
        std::mem::take(&mut *self.events.borrow_mut())
    }

    /// World centre of a pin, exactly where the dot is drawn.
    fn pin(&self, pin_id: i32) -> (f32, f32) {
        let geometry = self.geometry.borrow();
        let pin = geometry.pin(pin_id).expect("pin is cached");
        (pin.x, pin.y)
    }

    /// A point on the card body that carries no widget of its own: below
    /// the header and below the audio block when the card has one.
    fn body_point(&self, card: &NodeRow) -> (f32, f32) {
        let top = canvas::BODY_TOP
            + if card.has_audio_panel {
                canvas::AUDIO_BLOCK_HEIGHT
            } else {
                canvas::PORT_LIST_TOP
            };
        (card.x + card.width / 2.0, card.y + top + 8.0)
    }

    /// A point on the curve the canvas actually draws for `link_id`, read
    /// back out of the rendered SVG commands rather than recomputed, so the
    /// test aims at the same pixels the user sees.
    fn point_on_rendered_link(&self, link_id: i32, t: f32) -> (f32, f32) {
        let commands = self.window.invoke_graph_link_path(
            link_id,
            self.window.get_geometry_version(),
            0.0,
            0.0,
        );
        let numbers: Vec<f32> = commands
            .as_str()
            .split_whitespace()
            .filter_map(|token| token.parse::<f32>().ok())
            .collect();
        assert_eq!(numbers.len(), 8, "a cubic path: {commands}");
        let curve = [
            (numbers[0], numbers[1]),
            (numbers[2], numbers[3]),
            (numbers[4], numbers[5]),
            (numbers[6], numbers[7]),
        ];
        let u = 1.0 - t;
        let (a, b, c, d) = (u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t);
        (
            a * curve[0].0 + b * curve[1].0 + c * curve[2].0 + d * curve[3].0,
            a * curve[0].1 + b * curve[1].1 + c * curve[2].1 + d * curve[3].1,
        )
    }

    /// Create one link through the real pin-drag gesture and render it.
    fn create_link(&mut self) -> i32 {
        let (output, input) = self.connectable_pair();
        self.drag(self.pin(output), self.pin(input));
        for event in self.take_events() {
            process_event(&self.window, &mut self.application, event);
        }
        self.sync();
        let rendered = rows_of(&self.links);
        assert_eq!(rendered.len(), 1, "the new link reaches the render model");
        rendered[0].id
    }

    fn link_row(&self, link_id: i32) -> LinkRow {
        rows_of(&self.links)
            .into_iter()
            .find(|link| link.id == link_id)
            .expect("link is rendered")
    }

    fn node_row(&self, node_id: i32) -> NodeRow {
        rows_of(&self.nodes)
            .into_iter()
            .find(|node| node.id == node_id)
            .expect("node is rendered")
    }

    /// First output pin and first input pin of two different cards.
    fn connectable_pair(&self) -> (i32, i32) {
        let output = self
            .application
            .snapshot
            .nodes
            .iter()
            .find_map(|node| {
                node.ports
                    .iter()
                    .find(|port| port.direction != Direction::Sink)
                    .map(|port| (node.id, port.pin_id))
            })
            .expect("the demo graph has an output port");
        let input = self
            .application
            .snapshot
            .nodes
            .iter()
            .filter(|node| node.id != output.0)
            .find_map(|node| {
                node.ports
                    .iter()
                    .find(|port| port.direction == Direction::Sink)
                    .map(|port| port.pin_id)
            })
            .expect("the demo graph has an input port on another card");
        (output.1, input)
    }
}

#[test]
fn resolved_drop_is_the_command_position_and_round_trips_through_history() {
    let mut harness = CanvasHarness::new(ConnectMode::Advanced);
    let moving = harness.application.snapshot.nodes[0].clone();
    let obstacle = harness.application.snapshot.nodes[1].clone();
    let desired = [
        obstacle.position[0] - moving.position[0],
        obstacle.position[1] - moving.position[1],
    ];
    let selected = std::collections::BTreeSet::from([moving.node_id]);
    let resolved = resolve_drag_delta(&harness.application.snapshot, &selected, desired, true);
    assert_ne!(resolved, desired, "the requested drop overlaps an obstacle");
    let expected = [
        moving.position[0] + resolved[0],
        moving.position[1] + resolved[1],
    ];

    process_event(
        &harness.window,
        &mut harness.application,
        UiEvent::DragCommitted(moving.id, desired[0], desired[1]),
    );

    assert_eq!(
        harness
            .application
            .source
            .graph()
            .node(moving.node_id)
            .unwrap()
            .position,
        expected
    );
    harness.sync();
    let rendered = harness.node_row(moving.id);
    assert_eq!([rendered.x, rendered.y], expected);

    process_event(
        &harness.window,
        &mut harness.application,
        UiEvent::Action("undo".into()),
    );
    assert_eq!(
        harness
            .application
            .source
            .graph()
            .node(moving.node_id)
            .unwrap()
            .position,
        moving.position
    );

    process_event(
        &harness.window,
        &mut harness.application,
        UiEvent::Action("redo".into()),
    );
    assert_eq!(
        harness
            .application
            .source
            .graph()
            .node(moving.node_id)
            .unwrap()
            .position,
        expected
    );
}

/// Minimum width a button needs before `AppButton`'s label, inset by 9px
/// on each side, has room for a word. The relay rows used to place their
/// actions in 22px squares, where every glyph elided to a bare "…".
const READABLE_BUTTON_WIDTH: f32 = 60.0;

#[test]
fn relay_peer_rows_give_their_actions_a_readable_label() {
    let harness = CanvasHarness::new(ConnectMode::Advanced);
    harness.window.set_show_relay(true);
    // Discovery and client connections live under the PC → Phone direction;
    // tab 0 is now the Phone → PC host direction.
    harness.window.set_relay_tab(1);
    harness
        .window
        .set_relay_rows(ModelRc::from(Rc::new(VecModel::from(vec![RelayRow {
            id: "1".into(),
            name: "Configured peer".into(),
            address: "192.168.18.249:48123".into(),
            state: "configured".into(),
            level: 0.4,
            connected: true,
            connecting: false,
            trusted: true,
            peer_id: "abc".into(),
        }]))));
    slint::platform::update_timers_and_animations();

    let panel = i_slint_backend_testing::ElementHandle::find_by_element_type_name(
        &harness.window,
        "RelayPanel",
    )
    .next()
    .expect("the relay panel should be rendered");
    let panel_left = panel.absolute_position().x;
    let panel_right = panel_left + panel.size().width;

    // Every action inside the panel: the row's Connect/Forget pair, the
    // tab strip, the header close and the Connect button of the form.
    let buttons: Vec<_> = i_slint_backend_testing::ElementHandle::find_by_element_type_name(
        &harness.window,
        "AppButton",
    )
    .collect();
    assert!(buttons.len() >= 6, "expected the peer row's own actions");

    let row_actions: Vec<_> = buttons
        .iter()
        .filter(|button| button.size().height == 30.0)
        .collect();
    assert_eq!(row_actions.len(), 2, "connect and forget");
    for action in row_actions {
        let left = action.absolute_position().x;
        let right = left + action.size().width;
        assert!(
            action.size().width >= READABLE_BUTTON_WIDTH,
            "a peer action must fit its label, got {}",
            action.size().width
        );
        assert!(
            left >= panel_left && right <= panel_right,
            "a peer action must stay inside the panel"
        );
    }
}

#[test]
fn relay_sidebar_overlays_the_canvas_without_resizing_it() {
    let harness = CanvasHarness::new(ConnectMode::Advanced);
    let full_width = harness.window.get_canvas_width_();
    let pan = harness.window.get_pan_x();

    harness.window.set_show_relay(true);
    slint::platform::update_timers_and_animations();

    // The panel floats over the right half of the workspace: the canvas
    // keeps every pixel it had, so no node moves or reflows when it opens.
    assert!((harness.window.get_canvas_width_() - full_width).abs() < 0.01);
    assert!((harness.window.get_pan_x() - pan).abs() < 0.01);

    harness.window.set_show_relay(false);
    slint::platform::update_timers_and_animations();
    assert!((harness.window.get_canvas_width_() - full_width).abs() < 0.01);
    assert!((harness.window.get_pan_x() - pan).abs() < 0.01);
}

#[test]
fn dragging_between_two_rendered_pins_requests_that_exact_pair() {
    let harness = CanvasHarness::new(ConnectMode::Advanced);
    let (output, input) = harness.connectable_pair();

    harness.drag(harness.pin(output), harness.pin(input));

    let requested = harness
        .take_events()
        .into_iter()
        .find_map(|event| match event {
            UiEvent::LinkRequested(start, end) => Some((start, end)),
            _ => None,
        });
    assert_eq!(requested, Some((output, input)));
}

#[test]
fn a_pin_drag_released_over_empty_canvas_cancels_in_advanced_mode() {
    let harness = CanvasHarness::new(ConnectMode::Advanced);
    let (output, _) = harness.connectable_pair();

    harness.drag(harness.pin(output), (1150.0, 800.0));

    assert!(harness
        .take_events()
        .iter()
        .any(|event| matches!(event, UiEvent::LinkCancelled)));
}

#[test]
fn easy_mode_accepts_a_pin_drag_that_lands_anywhere_on_the_target_card() {
    let harness = CanvasHarness::new(ConnectMode::Easy);
    let (output, input) = harness.connectable_pair();
    let target = harness.pin(input);
    // Well clear of the pin dot, but still inside the destination card.
    let body = (target.0 + 70.0, target.1 + 6.0);

    harness.drag(harness.pin(output), body);

    let dropped = harness
        .take_events()
        .into_iter()
        .find_map(|event| match event {
            UiEvent::LinkDropped(pin, x, y) => Some((pin, x, y)),
            _ => None,
        });
    let (pin, x, y) = dropped.expect("easy mode routes the drop to the whole-card handler");
    assert_eq!(pin, output);
    assert!((x - body.0).abs() < 1.0 && (y - body.1).abs() < 1.0);
}

#[test]
fn easy_mode_connects_whole_cards_dragged_from_their_body() {
    let harness = CanvasHarness::new(ConnectMode::Easy);
    let (output, input) = harness.connectable_pair();
    let source_node = harness
        .geometry
        .borrow()
        .pin(output)
        .expect("pin is cached")
        .node_id;
    let source = harness.node_row(source_node);
    let from = harness.body_point(&source);

    harness.drag(from, harness.pin(input));

    let connected = harness
        .take_events()
        .into_iter()
        .find_map(|event| match event {
            UiEvent::NodeConnectDropped(id, _, _, target) => Some((id, target)),
            _ => None,
        });
    assert_eq!(connected, Some((source_node, input)));
}

#[test]
fn advanced_mode_moves_the_card_when_its_body_is_dragged() {
    let mut harness = CanvasHarness::new(ConnectMode::Advanced);
    let card = harness.node_row(harness.application.snapshot.nodes[0].id);
    let from = harness.body_point(&card);

    harness.drag(from, (from.0 + 40.0, from.1 + 25.0));

    let moved = harness
        .take_events()
        .into_iter()
        .find_map(|event| match event {
            UiEvent::DragCommitted(id, dx, dy) => Some((id, dx, dy)),
            _ => None,
        });
    let (id, dx, dy) = moved.expect("a body drag moves the card in advanced mode");
    assert_eq!(id, card.id);
    assert!((dx - 40.0).abs() < 0.1 && (dy - 25.0).abs() < 0.1);
    // The rendered row follows immediately, without waiting for a refresh.
    let after = harness.node_row(card.id);
    assert!((after.x - card.x - 40.0).abs() < 0.1);
    assert!((after.y - card.y - 25.0).abs() < 0.1);
    // ...and the cached pins moved with it, so the edges stay attached.
    let pin = harness.application.snapshot.nodes[0]
        .ports
        .first()
        .map(|port| port.pin_id);
    if let Some(pin) = pin {
        let cached = harness.geometry.borrow().pin(pin).expect("pin is cached");
        assert!((cached.x - after.x - canvas::PIN_INSET).abs() < card.width);
    }
    harness.sync();
}

#[test]
fn dragging_the_header_moves_the_whole_selection() {
    let harness = CanvasHarness::new(ConnectMode::Advanced);
    let first = harness.node_row(harness.application.snapshot.nodes[0].id);
    let second = harness.node_row(harness.application.snapshot.nodes[1].id);

    harness.click((first.x + 60.0, first.y + 12.0));
    harness.dispatch(WindowEvent::PointerPressed {
        position: harness.screen_of((second.x + 60.0, second.y + 12.0)),
        button: PointerEventButton::Left,
    });
    harness.dispatch(WindowEvent::PointerMoved {
        position: harness.screen_of((second.x + 60.0 + 30.0, second.y + 12.0 + 15.0)),
    });
    harness.dispatch(WindowEvent::PointerReleased {
        position: harness.screen_of((second.x + 60.0 + 30.0, second.y + 12.0 + 15.0)),
        button: PointerEventButton::Left,
    });

    // Clicking the second card replaced the selection, so only it moves.
    assert!((harness.node_row(second.id).x - second.x - 30.0).abs() < 0.1);
    assert!((harness.node_row(first.id).x - first.x).abs() < 0.1);
}

#[test]
fn clicking_a_card_selects_it_and_clicking_the_background_clears_it() {
    let harness = CanvasHarness::new(ConnectMode::Advanced);
    let card = harness.node_row(harness.application.snapshot.nodes[0].id);

    harness.click((card.x + 60.0, card.y + 12.0));
    assert!(harness.node_row(card.id).selected);
    assert!(harness
        .geometry
        .borrow()
        .node(card.id)
        .is_some_and(|node| node.selected));

    harness.click((1150.0, 820.0));
    assert!(!harness.node_row(card.id).selected);
}

#[test]
fn the_body_mute_button_keeps_its_own_pointer_gesture() {
    let harness = CanvasHarness::new(ConnectMode::Advanced);
    let card = harness
        .application
        .snapshot
        .nodes
        .iter()
        .find(|node| node.has_audio_controls)
        .map(|node| harness.node_row(node.id))
        .expect("the demo graph has a card with audio controls");

    // The mute button sits at the right of the audio block inside the body.
    harness.click((card.x + card.width - 22.0, card.y + canvas::BODY_TOP + 20.0));

    assert!(harness
        .take_events()
        .iter()
        .any(|event| matches!(event, UiEvent::ToggleAudioMute(id) if *id == card.id)));
}

#[test]
fn a_created_link_is_rendered_as_a_curve_between_its_two_pins() {
    let mut harness = CanvasHarness::new(ConnectMode::Advanced);
    let (output, input) = harness.connectable_pair();

    harness.drag(harness.pin(output), harness.pin(input));
    for event in harness.take_events() {
        process_event(&harness.window, &mut harness.application, event);
    }
    harness.sync();

    let rendered = rows_of(&harness.links);
    assert_eq!(rendered.len(), 1, "the new link reaches the render model");
    let commands = harness.window.invoke_graph_link_path(
        rendered[0].id,
        harness.window.get_geometry_version(),
        0.0,
        0.0,
    );
    let start = harness.pin(output);
    let end = harness.pin(input);
    assert!(
        commands.starts_with(&format!("M {:.2} {:.2} C ", start.0, start.1)),
        "the curve starts on the output pin: {commands}"
    );
    assert!(
        commands.ends_with(&format!(" {:.2} {:.2}", end.0, end.1)),
        "the curve ends on the input pin: {commands}"
    );
}

#[test]
fn clicking_between_curve_samples_selects_the_connection() {
    let mut harness = CanvasHarness::new(ConnectMode::Advanced);
    let link = harness.create_link();
    // 0.37 sits between the stops the old sampled hit test measured, which
    // is where a press used to fall through to the background.
    let on_curve = harness.point_on_rendered_link(link, 0.37);

    harness.click(on_curve);

    let selected = harness
        .take_events()
        .into_iter()
        .find_map(|event| match event {
            UiEvent::SelectLink(id, extend) => Some((id, extend)),
            _ => None,
        });
    assert_eq!(selected, Some((link, false)));
    assert!(
        harness.link_row(link).selected,
        "the rendered edge shows as selected"
    );
}

/// Dragging an existing edge onto a different pin re-routes it. Before
/// this the canvas had no link drag at all: a press on an edge only ever
/// selected it, so changing a connection meant deleting and redrawing.
#[test]
fn dragging_an_edge_onto_another_pin_reroutes_it() {
    let mut harness = CanvasHarness::new(ConnectMode::Advanced);
    let link = harness.create_link();
    let (output, input) = harness.connectable_pair();
    // A second sink on a different card to drop the input end onto.
    let other_input = harness
        .application
        .snapshot
        .nodes
        .iter()
        .filter(|node| {
            node.ports
                .iter()
                .all(|port| port.pin_id != input && port.pin_id != output)
        })
        .find_map(|node| {
            node.ports
                .iter()
                .find(|port| port.direction == Direction::Sink)
                .map(|port| port.pin_id)
        });
    let other_input = other_input.expect("the demo graph has a third card with an input");

    // Grab the edge near its input end so that end is the one that moves.
    let grab = harness.point_on_rendered_link(link, 0.92);
    harness.drag(grab, harness.pin(other_input));

    let rerouted = harness
        .take_events()
        .into_iter()
        .find_map(|event| match event {
            UiEvent::LinkRerouted(id, pin) => Some((id, pin)),
            _ => None,
        });
    assert_eq!(rerouted, Some((link, other_input)));

    for event in harness.take_events() {
        process_event(&harness.window, &mut harness.application, event);
    }
    harness.sync();
}

/// A press without movement stays a selection, so the new gesture does not
/// cost the old one.
#[test]
fn clicking_an_edge_still_only_selects_it() {
    let mut harness = CanvasHarness::new(ConnectMode::Advanced);
    let link = harness.create_link();

    harness.click(harness.point_on_rendered_link(link, 0.5));

    let events = harness.take_events();
    assert!(events
        .iter()
        .any(|event| matches!(event, UiEvent::SelectLink(id, _) if *id == link)));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, UiEvent::LinkRerouted(..))),
        "a click is not a re-route"
    );
    assert!(harness.link_row(link).selected);
}

#[test]
fn clicking_a_connection_at_half_zoom_selects_it() {
    let mut harness = CanvasHarness::new(ConnectMode::Advanced);
    let link = harness.create_link();
    harness.set_zoom(0.5);
    let on_curve = harness.point_on_rendered_link(link, 0.37);

    harness.click(on_curve);

    assert!(
        harness
            .take_events()
            .iter()
            .any(|event| matches!(event, UiEvent::SelectLink(id, _) if *id == link)),
        "a connection stays clickable when the canvas is zoomed out"
    );
    assert!(harness.link_row(link).selected);
}

/// The header is the move handle in both connect modes; only the blank
/// card body is an Easy-mode connect surface.
#[test]
fn easy_mode_header_drag_still_moves_the_card() {
    let mut harness = CanvasHarness::new(ConnectMode::Easy);
    let card = harness.node_row(harness.application.snapshot.nodes[0].id);
    let header = (card.x + 60.0, card.y + 12.0);

    harness.drag(header, (header.0 + 40.0, header.1 + 25.0));

    let events = harness.take_events();
    let moved = events.iter().find_map(|event| match event {
        UiEvent::DragCommitted(id, dx, dy) => Some((*id, *dx, *dy)),
        _ => None,
    });
    let (id, dx, dy) = moved.expect("the header stays a move handle in easy mode");
    assert_eq!(id, card.id);
    assert!((dx - 40.0).abs() < 0.1 && (dy - 25.0).abs() < 0.1);
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, UiEvent::NodeConnectDropped(..))),
        "the header never starts an easy-connect gesture"
    );
    harness.sync();
}

/// A collapsed card draws no pins, so its whole body used to land in the
/// `HIT_NODE` branch and connected only because Easy mode claimed every
/// `HIT_NODE`. Now that the header is a move handle again, the geometry has
/// to mark the body of a pinless card as a connect surface on its own.
#[test]
fn easy_mode_connects_a_collapsed_card_from_its_body() {
    let mut harness = CanvasHarness::new(ConnectMode::Easy);
    let (output, input) = harness.connectable_pair();
    let source_node = harness.geometry.borrow().pin(output).unwrap().node_id;
    harness.collapse(source_node);
    let card = harness.node_row(source_node);
    assert!(card.collapsed, "the card is collapsed");
    // Below the header, in the collapsed card's remaining strip.
    let body = (card.x + card.width / 2.0, card.y + card.height - 3.0);

    harness.drag(body, harness.pin(input));

    let events = harness.take_events();
    assert!(
        events.iter().any(
            |event| matches!(event, UiEvent::NodeConnectDropped(id, ..) if *id == source_node)
        ),
        "a collapsed card still connects from its body in easy mode"
    );
}

#[test]
fn advanced_mode_header_drag_moves_the_card() {
    let mut harness = CanvasHarness::new(ConnectMode::Advanced);
    let card = harness.node_row(harness.application.snapshot.nodes[0].id);
    let header = (card.x + 60.0, card.y + 12.0);

    harness.drag(header, (header.0 + 40.0, header.1 + 25.0));

    let moved = harness
        .take_events()
        .into_iter()
        .find_map(|event| match event {
            UiEvent::DragCommitted(id, dx, dy) => Some((id, dx, dy)),
            _ => None,
        });
    let (id, dx, dy) = moved.expect("the header is a move handle in advanced mode");
    assert_eq!(id, card.id);
    assert!((dx - 40.0).abs() < 0.1 && (dy - 25.0).abs() < 0.1);
    harness.sync();
}

#[test]
fn no_connect_backend_still_allows_link_selection() {
    let mut harness = CanvasHarness::new(ConnectMode::Advanced);
    let link = harness.create_link();
    let (output, input) = harness.connectable_pair();
    // What the UI does for a backend whose capabilities report connect and
    // disconnect as unsupported, such as Windows Core Audio.
    harness.window.set_connections_available(false);

    harness.click(harness.point_on_rendered_link(link, 0.37));
    assert!(
        harness
            .take_events()
            .iter()
            .any(|event| matches!(event, UiEvent::SelectLink(id, _) if *id == link)),
        "observed links stay selectable when routing is unsupported"
    );
    assert!(harness.link_row(link).selected);

    let card = harness.node_row(harness.application.snapshot.nodes[0].id);
    harness.click((card.x + 60.0, card.y + 12.0));
    assert!(
        harness
            .take_events()
            .iter()
            .any(|event| matches!(event, UiEvent::SelectNode(id, _) if *id == card.id)),
        "cards stay selectable too"
    );

    harness.drag(harness.pin(output), harness.pin(input));
    let events = harness.take_events();
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, UiEvent::LinkRequested(..))),
        "no routing is attempted"
    );
}

#[test]
fn the_background_grid_is_generated_for_the_visible_canvas() {
    let harness = CanvasHarness::new(ConnectMode::Advanced);

    harness.window.invoke_graph_request_grid();

    let grid = harness.window.get_grid_commands();
    assert!(!grid.is_empty(), "the canvas asks Rust for its grid lines");
    assert!(grid.starts_with("M "));
}

#[test]
fn easy_mode_pin_drag_connects_every_channel_of_the_two_groups() {
    let mut application = demo_application();
    application.view.connect_mode = ConnectMode::Easy;
    // In Easy mode the capture and playback cards each render one grouped
    // pin that stands for both FL and FR.
    let capture = application.view.ids.port(pw_graph_core::PortId(1)).unwrap();
    let playback = application.view.ids.port(pw_graph_core::PortId(3)).unwrap();

    handle_link_requested(&mut application, capture, playback);

    assert_eq!(application.source.graph().links.len(), 2);
    assert!(channels_are_paired_straight(&application));
}

#[test]
fn easy_mode_pin_drag_keeps_the_channels_straight_when_dragged_backwards() {
    let mut application = demo_application();
    application.view.connect_mode = ConnectMode::Easy;
    let capture = application.view.ids.port(pw_graph_core::PortId(1)).unwrap();
    let playback = application.view.ids.port(pw_graph_core::PortId(3)).unwrap();

    handle_link_requested(&mut application, playback, capture);

    assert_eq!(application.source.graph().links.len(), 2);
    assert!(channels_are_paired_straight(&application));
}

/// Every link joins two ports whose channel suffix is the same, so left
/// stays left and right stays right.
fn channels_are_paired_straight(application: &Application) -> bool {
    let graph = application.source.graph();
    graph.links.values().all(|link| {
        let output = graph.port(link.output_port).map(|port| port.name.clone());
        let input = graph.port(link.input_port).map(|port| port.name.clone());
        match (output, input) {
            (Some(output), Some(input)) => {
                let suffix = |name: &str| name.rsplit('_').next().unwrap_or_default().to_owned();
                suffix(&output) == suffix(&input)
            }
            _ => false,
        }
    })
}

#[test]
fn an_easy_mode_pin_drag_on_the_canvas_connects_both_channels() {
    let mut harness = CanvasHarness::new(ConnectMode::Easy);
    let (output, input) = harness.connectable_pair();

    harness.drag(harness.pin(output), harness.pin(input));
    for event in harness.take_events() {
        process_event(&harness.window, &mut harness.application, event);
    }
    harness.sync();

    // One grouped pin at each end, two channels, two links.
    assert_eq!(harness.application.source.graph().links.len(), 2);
    assert!(channels_are_paired_straight(&harness.application));
    assert_eq!(rows_of(&harness.links).len(), 2);
}

#[test]
fn an_easy_mode_card_drag_on_the_canvas_connects_both_channels() {
    let mut harness = CanvasHarness::new(ConnectMode::Easy);
    let (output, input) = harness.connectable_pair();
    let source = harness.node_row(
        harness
            .geometry
            .borrow()
            .pin(output)
            .expect("pin is cached")
            .node_id,
    );

    harness.drag(harness.body_point(&source), harness.pin(input));
    for event in harness.take_events() {
        process_event(&harness.window, &mut harness.application, event);
    }
    harness.sync();

    assert_eq!(harness.application.source.graph().links.len(), 2);
    assert!(channels_are_paired_straight(&harness.application));
}

#[test]
fn toggling_to_easy_mode_enables_body_connect() {
    let mut harness = CanvasHarness::new(ConnectMode::Advanced);
    let (output, input) = harness.connectable_pair();
    let source_node = harness.geometry.borrow().pin(output).unwrap().node_id;
    let source = harness.node_row(source_node);
    let target_node = harness.geometry.borrow().pin(input).unwrap().node_id;
    let target = harness.node_row(target_node);
    let body_point = (source.x + source.width / 2.0, harness.body_point(&source).1);

    let hit = harness
        .geometry
        .borrow()
        .hit_test(body_point.0, body_point.1, 1.0);
    assert_eq!(hit.kind, HIT_NODE, "advanced mode drags the card");

    handle_action(
        &harness.window,
        &mut harness.application,
        "toggle-connect-mode",
    );
    harness.sync();

    let hit = harness
        .geometry
        .borrow()
        .hit_test(body_point.0, body_point.1, 1.0);
    assert_eq!(
        hit.kind, HIT_NODE_BODY,
        "easy mode turns the body into a connect gesture"
    );

    let body = (
        target.x + target.width / 2.0,
        target.y + target.height / 2.0,
    );
    harness.drag(harness.body_point(&source), body);
    for event in harness.take_events() {
        process_event(&harness.window, &mut harness.application, event);
    }
    harness.sync();

    assert_eq!(
        harness.application.source.graph().links.len(),
        2,
        "links created after toggling to easy"
    );
    assert!(channels_are_paired_straight(&harness.application));
}

#[test]
fn pump_path_connects_body_to_body_after_toggling_to_easy() {
    i_slint_backend_testing::init_no_event_loop();
    let application = Rc::new(RefCell::new(demo_application()));
    application.borrow_mut().view.connect_mode = ConnectMode::Advanced;
    application.borrow_mut().view.pan = [0.0, 0.0];
    application.borrow_mut().view.zoom = 1.0;

    let window = MainWindow::new().unwrap();
    window
        .window()
        .set_size(slint::LogicalSize::new(1400.0, 900.0));
    let nodes = Rc::new(VecModel::default());
    let links = Rc::new(VecModel::default());
    window.set_nodes(ModelRc::from(nodes.clone()));
    window.set_links(ModelRc::from(links.clone()));
    let geometry = Rc::new(RefCell::new(CanvasGeometry::default()));
    let events = Rc::new(RefCell::new(Vec::new()));
    install_canvas_callbacks(&window, &nodes, &links, &geometry, &events);
    let minimap = Rc::new(VecModel::default());
    let shortcuts = Rc::new(VecModel::default());
    let version = Rc::new(Cell::new(0));

    let screen_of = |world: (f32, f32)| -> LogicalPosition {
        let application = application.borrow();
        LogicalPosition::new(
            RAIL_WIDTH + application.view.pan[0] + world.0,
            application.view.pan[1] + world.1,
        )
    };
    let body_of = |row: &NodeRow| -> (f32, f32) {
        let top = canvas::BODY_TOP
            + if row.has_audio_panel {
                canvas::AUDIO_BLOCK_HEIGHT
            } else {
                canvas::PORT_LIST_TOP
            };
        (row.x + row.width / 2.0, row.y + top + 8.0)
    };

    // First pump establishes advanced geometry, exactly like app startup.
    pump(
        &window,
        &application,
        &nodes,
        &links,
        &minimap,
        &shortcuts,
        &events,
        &geometry,
        &version,
    );

    // Toggle to Easy through the same events queue the toolbar uses.
    events
        .borrow_mut()
        .push(UiEvent::Action("toggle-connect-mode".into()));
    pump(
        &window,
        &application,
        &nodes,
        &links,
        &minimap,
        &shortcuts,
        &events,
        &geometry,
        &version,
    );

    let (output, input) = {
        let application = application.borrow();
        let output = application
            .snapshot
            .nodes
            .iter()
            .find_map(|node| {
                node.ports
                    .iter()
                    .find(|port| port.direction != Direction::Sink)
                    .map(|port| port.pin_id)
            })
            .expect("the demo graph has an output port");
        let input = application
            .snapshot
            .nodes
            .iter()
            .find_map(|node| {
                node.ports
                    .iter()
                    .find(|port| port.direction == Direction::Sink)
                    .map(|port| port.pin_id)
            })
            .expect("the demo graph has an input port on another card");
        (output, input)
    };
    let source_node = geometry
        .borrow()
        .pin(output)
        .expect("source pin cached")
        .node_id;
    let target_node = geometry
        .borrow()
        .pin(input)
        .expect("target pin cached")
        .node_id;
    let source = rows_of(&nodes)
        .into_iter()
        .find(|row| row.id == source_node)
        .unwrap();
    let target = rows_of(&nodes)
        .into_iter()
        .find(|row| row.id == target_node)
        .unwrap();
    let body = (
        target.x + target.width / 2.0,
        target.y + target.height / 2.0,
    );

    window.window().dispatch_event(WindowEvent::PointerPressed {
        position: screen_of(body_of(&source)),
        button: PointerEventButton::Left,
    });
    slint::platform::update_timers_and_animations();
    window.window().dispatch_event(WindowEvent::PointerMoved {
        position: screen_of(body),
    });
    slint::platform::update_timers_and_animations();
    window
        .window()
        .dispatch_event(WindowEvent::PointerReleased {
            position: screen_of(body),
            button: PointerEventButton::Left,
        });
    slint::platform::update_timers_and_animations();
    for event in std::mem::take(&mut *events.borrow_mut()) {
        process_event(&window, &mut application.borrow_mut(), event);
    }
    pump(
        &window,
        &application,
        &nodes,
        &links,
        &minimap,
        &shortcuts,
        &events,
        &geometry,
        &version,
    );

    assert_eq!(
        application.borrow().source.graph().links.len(),
        2,
        "links created"
    );
    assert!(channels_are_paired_straight(&application.borrow()));
}

#[test]
fn body_drag_released_on_a_target_pin_still_connects_the_whole_group() {
    let mut harness = CanvasHarness::new(ConnectMode::Easy);
    let (output, input) = harness.connectable_pair();
    let source_node = harness.geometry.borrow().pin(output).unwrap().node_id;
    let source = harness.node_row(source_node);

    // Release the whole-card drag exactly on the target node's rendered
    // pin: the group under it must still pair both stereo channels.
    let drop = harness.pin(input);
    harness.drag(harness.body_point(&source), drop);
    for event in harness.take_events() {
        process_event(&harness.window, &mut harness.application, event);
    }
    harness.sync();

    assert_eq!(
        harness.application.source.graph().links.len(),
        2,
        "drop on a pin fills the whole group"
    );
    assert!(channels_are_paired_straight(&harness.application));
}

#[cfg(feature = "relay")]
#[test]
fn refused_trusted_candidate_is_skipped_until_rediscovered() {
    use pw_graph_backend::{RelayDeviceKind, RelayPeerInfo};
    use pw_graph_config::PersistedRelayPeer;
    use pw_graph_utils::hex::hex_encode;

    let mut application = demo_application();
    application
        .config
        .relay_trusted_peers
        .push(PersistedRelayPeer {
            peer_id: "phone-id".into(),
            secret: hex_encode(&[7u8; 32]),
            name: "phone".into(),
            address: "192.0.2.10:48123".into(),
        });
    let peer = RelayPeerInfo {
        id: "phone-id".into(),
        name: "phone".into(),
        kind: RelayDeviceKind::Other,
        addr: "192.0.2.10:48123".parse().unwrap(),
    };
    assert!(super::relay::trusted_candidate_allowed(
        &mut application,
        &peer
    ));

    // A refused dial marks the address; without a re-announce the retry
    // loop must stop chasing it.
    super::relay::note_trusted_candidate_refused(
        &mut application,
        "phone-id",
        "192.0.2.10:48123",
    );
    assert!(!super::relay::trusted_candidate_allowed(
        &mut application,
        &peer
    ));

    // Discovery re-announcing (or a successful session) revives it.
    super::relay::clear_trusted_candidate_refused(
        &mut application,
        "phone-id",
        "192.0.2.10:48123",
    );
    assert!(super::relay::trusted_candidate_allowed(
        &mut application,
        &peer
    ));
}

#[cfg(feature = "relay")]
#[test]
fn trust_marks_are_scoped_per_peer_and_address() {
    use pw_graph_backend::{RelayDeviceKind, RelayPeerInfo};

    let mut application = demo_application();
    let refused = RelayPeerInfo {
        id: "phone-id".into(),
        name: "phone".into(),
        kind: RelayDeviceKind::Other,
        addr: "192.0.2.10:48123".parse().unwrap(),
    };
    let other_address = RelayPeerInfo {
        addr: "192.0.2.99:48123".parse().unwrap(),
        ..refused.clone()
    };
    super::relay::note_trusted_candidate_refused(
        &mut application,
        "phone-id",
        "192.0.2.10:48123",
    );
    assert!(!super::relay::trusted_candidate_allowed(
        &mut application,
        &refused
    ));
    // A different address for the same peer stays usable.
    assert!(super::relay::trusted_candidate_allowed(
        &mut application,
        &other_address
    ));
}

#[cfg(feature = "relay")]
#[test]
fn reenrollment_dialog_applies_only_to_first_contact() {
    use pw_graph_config::PersistedRelayPeer;
    use pw_graph_utils::hex::hex_encode;

    let mut application = demo_application();
    assert!(!super::relay::is_trusted_peer(&application, "phone-id"));
    assert!(!super::relay::is_trusted_peer(&application, "   "));

    application
        .config
        .relay_trusted_peers
        .push(PersistedRelayPeer {
            peer_id: "phone-id".into(),
            secret: hex_encode(&[7u8; 32]),
            name: "phone".into(),
            address: "192.0.2.10:48123".into(),
        });
    // An already-stored peer rotating its secret re-accepts silently; the
    // dialog is reserved for first contact.
    assert!(super::relay::is_trusted_peer(&application, "phone-id"));
    assert!(!super::relay::is_trusted_peer(&application, "stranger-id"));
}
