use super::*;
use pw_graph_core::{LinkId, Node, Port};

fn graph() -> Graph {
    let mut graph = Graph::default();
    let mut source = Node::new(NodeId(1), "Source", NodeType::PipeWire);
    source.position = [10.0, 20.0];
    graph.add_node(source).unwrap();
    graph
        .add_node(Node::new(NodeId(2), "Sink", NodeType::PipeWire))
        .unwrap();
    graph
        .add_port(Port::new(
            PortId(u64::MAX),
            NodeId(1),
            "output_FL",
            Direction::Source,
            PortType::Audio,
        ))
        .unwrap();
    graph
        .add_port(Port::new(
            PortId(2),
            NodeId(2),
            "input_FL",
            Direction::Sink,
            PortType::Audio,
        ))
        .unwrap();
    graph
        .add_link(LinkId(7), PortId(u64::MAX), PortId(2))
        .unwrap();
    graph
}

#[test]
fn id_map_never_casts_large_backend_ids() {
    let graph = graph();
    let mut ids = SlintIdMap::default();
    ids.rebuild(&graph);
    assert_ne!(ids.port(PortId(u64::MAX)).unwrap() as u64, u64::MAX);
    assert_eq!(ids.node_id(ids.node(NodeId(1)).unwrap()), Some(NodeId(1)));
    assert_eq!(
        ids.port_id(ids.port(PortId(u64::MAX)).unwrap()),
        Some(PortId(u64::MAX))
    );
}

#[test]
fn snapshot_filters_and_keeps_link_endpoints() {
    let graph = graph();
    let config = AppConfig::default();
    let mut state = UiGraphState::from_config(&config);
    let snapshot = state.snapshot(&graph, &config);
    assert_eq!(snapshot.nodes.len(), 2);
    assert_eq!(snapshot.links.len(), 1);
    state.media_filter = MediaFilter::Midi;
    assert!(state.snapshot(&graph, &config).nodes.is_empty());
}

#[test]
fn box_selection_includes_links_by_endpoint() {
    let graph = graph();
    let config = AppConfig::default();
    let mut state = UiGraphState::from_config(&config);
    let snapshot = state.snapshot(&graph, &config);

    state.select_box(&snapshot, -1000.0, -1000.0, 5000.0, 5000.0, false);

    assert!(state.selected_nodes.contains(&NodeId(1)));
    assert!(state.selected_nodes.contains(&NodeId(2)));
    assert!(state.selected_links.contains(&LinkId(7)));
}

#[test]
fn projection_does_not_rewrite_configured_overlaps() {
    let graph = graph();
    let mut config = AppConfig::default();
    config.node_positions.insert("1".into(), [0.0, 0.0]);
    config.node_positions.insert("2".into(), [0.0, 0.0]);
    config.repel_overlapping_nodes = true;
    let mut state = UiGraphState::from_config(&config);
    let snapshot = state.snapshot(&graph, &config);

    assert!(snapshot
        .nodes
        .iter()
        .all(|node| node.position == [0.0, 0.0]));
}

#[test]
fn overlapping_drop_chooses_the_nearest_card_edge() {
    let graph = graph();
    let config = AppConfig::default();
    let mut state = UiGraphState::from_config(&config);
    let mut snapshot = state.snapshot(&graph, &config);
    snapshot.nodes[0].position = [0.0, 0.0];
    snapshot.nodes[0].width = 100.0;
    snapshot.nodes[0].height = 100.0;
    snapshot.nodes[1].position = [100.0, 100.0];
    snapshot.nodes[1].width = 100.0;
    snapshot.nodes[1].height = 100.0;
    let selected = BTreeSet::from([snapshot.nodes[0].node_id]);

    let resolved = resolve_drag_delta(&snapshot, &selected, [100.0, 100.0], true);

    // Above and left are equally near; the stable tie-breaker picks above.
    assert_eq!(resolved, [100.0, -18.0]);
    assert!(drag_is_clear(
        &[&snapshot.nodes[0]],
        &[&snapshot.nodes[1]],
        resolved
    ));
}

#[test]
fn selected_nodes_resolve_as_one_rigid_group() {
    let graph = graph();
    let config = AppConfig::default();
    let mut state = UiGraphState::from_config(&config);
    let mut snapshot = state.snapshot(&graph, &config);
    let mut obstacle = snapshot.nodes[1].clone();
    snapshot.nodes[0].position = [0.0, 0.0];
    snapshot.nodes[1].position = [150.0, 0.0];
    obstacle.node_id = NodeId(99);
    obstacle.id = 99;
    obstacle.position = [300.0, 0.0];
    snapshot.nodes[0].width = 100.0;
    snapshot.nodes[0].height = 100.0;
    snapshot.nodes[1].width = 100.0;
    snapshot.nodes[1].height = 100.0;
    obstacle.width = 100.0;
    obstacle.height = 100.0;
    snapshot.nodes.push(obstacle);
    let selected = BTreeSet::from([NodeId(1), NodeId(2)]);

    let resolved = resolve_drag_delta(&snapshot, &selected, [200.0, 0.0], true);

    assert_eq!(resolved, [200.0, -118.0]);
    let moved = snapshot
        .nodes
        .iter()
        .filter(|node| selected.contains(&node.node_id))
        .map(|node| {
            [
                node.position[0] + resolved[0],
                node.position[1] + resolved[1],
            ]
        })
        .collect::<Vec<_>>();
    assert_eq!(moved[1][0] - moved[0][0], 150.0);
    assert_eq!(moved[1][1] - moved[0][1], 0.0);
}

#[test]
fn filtered_nodes_are_not_drag_obstacles() {
    let mut graph = graph();
    let mut hidden = Node::new(NodeId(3), "Hidden MIDI", NodeType::AlsaMidi);
    hidden.position = [300.0, 0.0];
    graph.add_node(hidden).unwrap();
    graph
        .add_port(Port::new(
            PortId(3),
            NodeId(3),
            "midi",
            Direction::Source,
            PortType::MidiAlsa,
        ))
        .unwrap();
    let mut config = AppConfig::default();
    config.node_positions.insert("1".into(), [0.0, 0.0]);
    config.node_positions.insert("2".into(), [1000.0, 0.0]);
    config.node_positions.insert("3".into(), [300.0, 0.0]);
    let mut state = UiGraphState::from_config(&config);
    state.media_filter = MediaFilter::Audio;
    let snapshot = state.snapshot(&graph, &config);
    assert!(!snapshot.nodes.iter().any(|node| node.node_id == NodeId(3)));
    let selected = BTreeSet::from([NodeId(1)]);

    assert_eq!(
        resolve_drag_delta(&snapshot, &selected, [300.0, 0.0], true),
        [300.0, 0.0]
    );
}

#[test]
fn disabled_repulsion_preserves_the_exact_requested_delta() {
    let graph = graph();
    let config = AppConfig::default();
    let mut state = UiGraphState::from_config(&config);
    let snapshot = state.snapshot(&graph, &config);
    let selected = BTreeSet::from([snapshot.nodes[0].node_id]);

    assert_eq!(
        resolve_drag_delta(&snapshot, &selected, [123.0, -45.0], false),
        [123.0, -45.0]
    );
}

#[test]
fn local_positions_are_explicitly_written_to_config() {
    let graph = graph();
    let mut config = AppConfig::default();
    let mut state = UiGraphState::from_config(&config);
    let snapshot = state.snapshot(&graph, &config);
    state.set_local_position(snapshot.nodes[0].id, 99.0, 101.0);
    assert_eq!(
        state.snapshot(&graph, &config).nodes[0].position,
        [99.0, 101.0]
    );
    assert!(config.node_positions.is_empty());

    state.write_to_config(&graph, &mut config);

    assert_eq!(config.node_positions.get("1"), Some(&[99.0, 101.0]));
    assert_eq!(
        config.node_positions_by_name.get("PipeWire:Source"),
        Some(&[99.0, 101.0])
    );
}

/// The UI must not draw a control the node's backend cannot provide. A
/// Windows application session that exposes no volume, or an effect node
/// with no controls at all, has to come out of the projection without an
/// audio block rather than with a dead fader.
#[test]
fn cards_hide_controls_the_backend_does_not_support() {
    let graph = graph();
    let config = AppConfig::default();
    let mut state = UiGraphState::from_config(&config);
    let uncontrollable = graph
        .nodes
        .keys()
        .map(|node_id| {
            (
                *node_id,
                NodeBackendProfile {
                    state: NodeAudioState::UNSUPPORTED,
                    capabilities: NodeCapabilities::NONE,
                    connectable: false,
                },
            )
        })
        .collect();

    let snapshot = state.snapshot_with_meters(
        &graph,
        &config,
        &BTreeMap::new(),
        MeterState::Unavailable,
        &uncontrollable,
    );

    assert!(
        snapshot.nodes.iter().all(|node| !node.has_audio_controls),
        "no card claims controls the backend cannot serve"
    );
    assert!(
        snapshot
            .nodes
            .iter()
            .all(|node| node.audio.state.volume.is_none()),
        "and none of them carries an invented level"
    );
}

/// The same graph with a capable backend keeps its controls, so the gate
/// above is really reading capability and not just switching everything off.
#[test]
fn cards_show_controls_a_capable_backend_reports() {
    let graph = graph();
    let config = AppConfig::default();
    let mut state = UiGraphState::from_config(&config);

    let snapshot = state.snapshot(&graph, &config);

    let audio_cards = snapshot
        .nodes
        .iter()
        .filter(|node| node.has_audio_controls)
        .count();
    assert!(audio_cards > 0, "a capable backend still gets audio cards");
    assert!(snapshot
        .nodes
        .iter()
        .filter(|node| node.has_audio_controls)
        .all(|node| node.audio.capabilities.volume_write));
}

#[test]
fn unknown_mute_is_not_projected_as_unmuted() {
    let graph = graph();
    let config = AppConfig::default();
    let mut state = UiGraphState::from_config(&config);
    let profiles = graph
        .nodes
        .keys()
        .map(|node_id| {
            (
                *node_id,
                NodeBackendProfile {
                    state: NodeAudioState {
                        volume: Some(0.5),
                        volume_readable: true,
                        volume_writable: true,
                        mute_writable: true,
                        ..NodeAudioState::UNSUPPORTED
                    },
                    capabilities: NodeCapabilities {
                        volume_read: true,
                        volume_write: true,
                        mute_read: false,
                        mute_write: true,
                        ..NodeCapabilities::NONE
                    },
                    connectable: true,
                },
            )
        })
        .collect();

    let snapshot = state.snapshot_with_meters(
        &graph,
        &config,
        &BTreeMap::new(),
        MeterState::Unavailable,
        &profiles,
    );
    assert!(snapshot
        .nodes
        .iter()
        .all(|node| node.audio.state.muted.is_none()));
    assert!(snapshot
        .nodes
        .iter()
        .all(|node| node.audio.state.muted != Some(true)));
}

#[test]
fn known_mute_values_are_distinct_from_unknown() {
    let graph = graph();
    let config = AppConfig::default();
    let mut state = UiGraphState::from_config(&config);
    let mut profiles = fully_capable_backend_profiles(&graph);
    profiles.get_mut(&NodeId(1)).unwrap().state.muted = Some(false);
    profiles.get_mut(&NodeId(2)).unwrap().state.muted = Some(true);
    let snapshot = state.snapshot_with_meters(
        &graph,
        &config,
        &BTreeMap::new(),
        MeterState::Unavailable,
        &profiles,
    );
    let source = snapshot
        .nodes
        .iter()
        .find(|node| node.node_id == NodeId(1))
        .unwrap();
    let sink = snapshot
        .nodes
        .iter()
        .find(|node| node.node_id == NodeId(2))
        .unwrap();
    assert_eq!(source.audio.state.muted, Some(false));
    assert_eq!(sink.audio.state.muted, Some(true));
}

#[test]
fn meter_only_and_peak_only_nodes_get_an_independent_panel() {
    let graph = graph();
    let config = AppConfig::default();
    let mut state = UiGraphState::from_config(&config);
    let profiles = graph
        .nodes
        .keys()
        .map(|node_id| {
            (
                *node_id,
                NodeBackendProfile {
                    state: NodeAudioState::UNSUPPORTED,
                    capabilities: NodeCapabilities {
                        meter_peak: true,
                        ..NodeCapabilities::NONE
                    },
                    connectable: false,
                },
            )
        })
        .collect();
    let snapshot = state.snapshot_with_meters(
        &graph,
        &config,
        &BTreeMap::new(),
        MeterState::Waiting,
        &profiles,
    );
    assert!(snapshot
        .nodes
        .iter()
        .all(|node| !node.has_audio_controls && node.has_audio_panel));
    assert!(snapshot
        .nodes
        .iter()
        .all(|node| node.audio.capabilities.meter_peak && !node.audio.capabilities.meter_rms));
    assert!(snapshot
        .nodes
        .iter()
        .all(|node| node.meter.state == MeterState::Waiting));
}

#[test]
fn thumbnail_projection_matches_the_compact_canvas_mode() {
    let graph = graph();
    let config = AppConfig::default();
    let mut state = UiGraphState::from_config(&config);
    state.thumbnail_mode = true;

    let snapshot = state.snapshot(&graph, &config);

    assert!(snapshot
        .nodes
        .iter()
        .all(|node| node.thumbnail && node.height == 62.0));
    assert!(snapshot.links.is_empty());
}

#[test]
fn collapse_is_restored_through_config() {
    let graph = graph();
    let mut config = AppConfig::default();
    let mut state = UiGraphState::from_config(&config);
    let snapshot = state.snapshot(&graph, &config);

    state.toggle_local_collapse(snapshot.nodes[0].id, &snapshot);

    assert!(state.snapshot(&graph, &config).nodes[0].collapsed);
    assert!(config.node_view_by_name.is_empty());

    state.write_to_config(&graph, &mut config);
    let mut restored = UiGraphState::from_config(&config);
    assert!(restored.snapshot(&graph, &config).nodes[0].collapsed);
}

#[test]
fn supplied_meters_are_projected_without_changing_the_graph() {
    let graph = graph();
    let config = AppConfig::default();
    let mut state = UiGraphState::from_config(&config);
    let mut meters = BTreeMap::new();
    meters.insert(
        NodeId(1),
        MeterReading {
            rms: 0.42,
            peak: 0.81,
            state: MeterState::Live,
        },
    );

    let snapshot = state.snapshot_with_meters(
        &graph,
        &config,
        &meters,
        MeterState::Waiting,
        &fully_capable_backend_profiles(&graph),
    );
    let source = snapshot
        .nodes
        .iter()
        .find(|node| node.node_id == NodeId(1))
        .unwrap();
    let sink = snapshot
        .nodes
        .iter()
        .find(|node| node.node_id == NodeId(2))
        .unwrap();

    assert_eq!(source.meter.rms, 0.42);
    assert_eq!(source.meter.peak, 0.81);
    assert_eq!(source.meter.state, MeterState::Live);
    assert_eq!(sink.meter.state, MeterState::Waiting);
    assert_eq!(graph.links.len(), 1);
}

#[test]
fn easy_mode_pairs_a_source_into_the_relay_speaker() {
    // Relay endpoints are ordinary patchable nodes: an Easy-mode gesture
    // has to resolve pairs into them like it does for any other card.
    let mut graph = graph();
    graph
        .add_node(Node::new(NodeId(3), RELAY_SINK_NAME, NodeType::PipeWire))
        .unwrap();
    for (id, channel) in [(PortId(30), "FL"), (PortId(31), "FR")] {
        graph
            .add_port(
                Port::new(id, NodeId(3), channel, Direction::Sink, PortType::Audio)
                    .with_channel(channel),
            )
            .unwrap();
    }
    let config = AppConfig::default();
    let mut state = UiGraphState::from_config(&config);
    state.relay_nodes_visible = true;

    let pairs = state.matching_port_pairs(&graph, NodeId(1), NodeId(3));
    assert_eq!(pairs, vec![(PortId(u64::MAX), PortId(30))]);

    let snapshot = state.snapshot_with_meters(
        &graph,
        &config,
        &BTreeMap::new(),
        MeterState::Waiting,
        &fully_capable_backend_profiles(&graph),
    );
    let relay = snapshot
        .nodes
        .iter()
        .find(|node| node.node_id == NodeId(3))
        .unwrap();
    assert!(relay.connectable);
    assert_eq!(relay.ports.len(), 2);
}

#[test]
fn easy_mode_groups_the_relay_stereo_pair_into_one_pin() {
    // The relay filters name their ports `<role>_<channel>` so the canvas
    // can strip the channel suffix; bare "FL"/"FR" has no base name and
    // used to leave two loose pins on the card in Easy mode.
    let mut graph = graph();
    graph
        .add_node(Node::new(NodeId(3), RELAY_SINK_NAME, NodeType::PipeWire))
        .unwrap();
    for (id, channel) in [(PortId(30), "FL"), (PortId(31), "FR")] {
        graph
            .add_port(
                Port::new(
                    id,
                    NodeId(3),
                    format!("playback_{channel}"),
                    Direction::Sink,
                    PortType::Audio,
                )
                .with_channel(channel),
            )
            .unwrap();
    }
    let config = AppConfig::default();
    let mut state = UiGraphState::from_config(&config);
    state.relay_nodes_visible = true;

    state.connect_mode = ConnectMode::Easy;
    let grouped = state.snapshot(&graph, &config);
    let relay = grouped
        .nodes
        .iter()
        .find(|node| node.node_id == NodeId(3))
        .unwrap();
    assert_eq!(relay.ports.len(), 1);
    assert_eq!(relay.ports[0].label, "playback");
    assert_eq!(relay.ports[0].ports.len(), 2);

    state.connect_mode = ConnectMode::Advanced;
    let individual = state.snapshot(&graph, &config);
    let relay = individual
        .nodes
        .iter()
        .find(|node| node.node_id == NodeId(3))
        .unwrap();
    assert_eq!(relay.ports.len(), 2);
}

#[test]
fn relay_nodes_are_hidden_until_relay_activity_starts() {
    let mut graph = graph();
    graph
        .add_node(Node::new(NodeId(3), RELAY_SOURCE_NAME, NodeType::PipeWire))
        .unwrap();
    graph
        .add_node(Node::new(NodeId(4), RELAY_SINK_NAME, NodeType::PipeWire))
        .unwrap();
    let config = AppConfig::default();
    let mut state = UiGraphState::from_config(&config);

    let hidden = state.snapshot(&graph, &config);
    assert_eq!(hidden.nodes.len(), 2);
    assert!(hidden.nodes.iter().all(|node| node.node_id.0 < 3));

    state.relay_nodes_visible = true;
    let active = state.snapshot(&graph, &config);
    assert_eq!(active.nodes.len(), 4);
    assert!(active.nodes.iter().any(|node| node.node_id == NodeId(3)));
    assert!(active.nodes.iter().any(|node| node.node_id == NodeId(4)));
}

#[test]
fn easy_mode_pairs_every_effect_node_in_either_drag_direction() {
    let mut graph = graph();
    graph
        .add_port(
            Port::new(
                PortId(3),
                NodeId(1),
                "output_FR",
                Direction::Source,
                PortType::Audio,
            )
            .with_channel("FR"),
        )
        .unwrap();
    for (node_id, first_port) in [(NodeId(3), 30), (NodeId(4), 40)] {
        graph
            .add_node(Node::new(
                node_id,
                format!("Effect {}", node_id.0),
                NodeType::Effect,
            ))
            .unwrap();
        for (offset, name, direction, channel) in [
            (0, "input_FL", Direction::Sink, "FL"),
            (1, "input_FR", Direction::Sink, "FR"),
            (2, "output_FL", Direction::Source, "FL"),
            (3, "output_FR", Direction::Source, "FR"),
        ] {
            graph
                .add_port(
                    Port::new(
                        PortId(first_port + offset),
                        node_id,
                        name,
                        direction,
                        PortType::Audio,
                    )
                    .with_channel(channel),
                )
                .unwrap();
        }
    }
    let config = AppConfig {
        connect_mode: "easy".into(),
        ..AppConfig::default()
    };
    let state = UiGraphState::from_config(&config);

    for effect in [NodeId(3), NodeId(4)] {
        let into_effect = state.matching_port_pairs(&graph, NodeId(1), effect);
        assert_eq!(into_effect.len(), 2);
        let from_effect_to_sink = state.matching_port_pairs(&graph, NodeId(2), effect);
        assert_eq!(from_effect_to_sink.len(), 1);
        let output = graph.port(from_effect_to_sink[0].0).unwrap();
        let input = graph.port(from_effect_to_sink[0].1).unwrap();
        assert_eq!(output.node_id, effect);
        assert_eq!(input.node_id, NodeId(2));
    }
}

/// Two stereo cards, where the sink exposes two separate stereo groups.
fn stereo_graph() -> Graph {
    let mut graph = Graph::default();
    graph
        .add_node(Node::new(NodeId(1), "Capture", NodeType::PipeWire))
        .unwrap();
    graph
        .add_node(Node::new(NodeId(2), "Playback", NodeType::PipeWire))
        .unwrap();
    let ports = [
        (1, NodeId(1), "capture_FL", Direction::Source, "FL"),
        (2, NodeId(1), "capture_FR", Direction::Source, "FR"),
        (3, NodeId(2), "main_FL", Direction::Sink, "FL"),
        (4, NodeId(2), "main_FR", Direction::Sink, "FR"),
        (5, NodeId(2), "aux_FL", Direction::Sink, "FL"),
        (6, NodeId(2), "aux_FR", Direction::Sink, "FR"),
    ];
    for (id, node, name, direction, channel) in ports {
        graph
            .add_port(
                Port::new(PortId(id), node, name, direction, PortType::Audio).with_channel(channel),
            )
            .unwrap();
    }
    graph
}

fn easy_state() -> UiGraphState {
    let config = AppConfig {
        connect_mode: "easy".into(),
        ..AppConfig::default()
    };
    UiGraphState::from_config(&config)
}

#[test]
fn easy_mode_groups_a_stereo_pair_behind_one_pin() {
    let graph = stereo_graph();
    let mut state = easy_state();
    state.ids.rebuild(&graph);
    let capture = state.ids.port(PortId(1)).unwrap();

    let group = state.port_group(&graph, capture).unwrap();

    assert_eq!(group.label, "capture");
    assert_eq!(group.ports, vec![PortId(1), PortId(2)]);
}

#[test]
fn drag_collision_uses_the_projected_easy_mode_height() {
    let graph = stereo_graph();
    let mut config = AppConfig {
        connect_mode: "easy".into(),
        ..AppConfig::default()
    };
    config.node_positions.insert("1".into(), [0.0, 0.0]);
    config.node_positions.insert("2".into(), [0.0, 400.0]);
    let mut state = UiGraphState::from_config(&config);
    let mut snapshot = state.snapshot(&graph, &config);
    let moving = snapshot
        .nodes
        .iter()
        .position(|node| node.node_id == NodeId(1))
        .unwrap();
    let obstacle = snapshot
        .nodes
        .iter()
        .position(|node| node.node_id == NodeId(2))
        .unwrap();
    let projected_height = snapshot.nodes[moving].height;
    let raw_height = node_height(
        false,
        false,
        true,
        graph.node(NodeId(1)).unwrap().ports.len(),
    );
    assert!(raw_height > projected_height);
    snapshot.nodes[obstacle].position = [0.0, projected_height + COLLISION_GAP + 1.0];
    let selected = BTreeSet::from([NodeId(1)]);

    assert_eq!(
        resolve_drag_delta(&snapshot, &selected, [0.0, 0.0], true),
        [0.0, 0.0]
    );
}

#[test]
fn easy_pin_pairs_keep_left_on_left_in_both_drag_directions() {
    let graph = stereo_graph();
    let mut state = easy_state();
    state.ids.rebuild(&graph);
    let capture = state.ids.port(PortId(1)).unwrap();
    let main = state.ids.port(PortId(3)).unwrap();

    let forward = state.matching_pin_pairs(&graph, capture, main);
    let backward = state.matching_pin_pairs(&graph, main, capture);

    assert_eq!(
        forward,
        vec![(PortId(1), PortId(3)), (PortId(2), PortId(4))]
    );
    assert_eq!(backward, forward);
}

#[test]
fn easy_pin_pairs_refuse_two_pins_that_face_the_same_way() {
    let graph = stereo_graph();
    let mut state = easy_state();
    state.ids.rebuild(&graph);
    let main = state.ids.port(PortId(3)).unwrap();
    let aux = state.ids.port(PortId(5)).unwrap();

    assert!(state.matching_pin_pairs(&graph, main, aux).is_empty());
}

#[test]
fn a_group_dropped_on_a_card_only_fills_one_of_its_groups() {
    let graph = stereo_graph();
    let mut state = easy_state();
    state.ids.rebuild(&graph);
    let capture = state.ids.port(PortId(1)).unwrap();

    let pairs = state.matching_group_to_node_pairs(&graph, capture, NodeId(2));

    // Both channels land, both in the same destination group (the first
    // one the card renders), and left stays on left.
    assert_eq!(pairs.len(), 2);
    let base = |port: PortId| channel_base_name(graph.port(port).unwrap()).unwrap();
    let channel = |port: PortId| channel_identity(graph.port(port).unwrap()).unwrap();
    assert_eq!(base(pairs[0].1), base(pairs[1].1));
    assert!(pairs
        .iter()
        .all(|(output, input)| channel(*output) == channel(*input)));
    assert_ne!(pairs[0].1, pairs[1].1);
}

#[test]
fn a_mono_endpoint_still_connects_to_a_stereo_one() {
    let mut graph = stereo_graph();
    graph
        .add_node(Node::new(NodeId(3), "Mono sink", NodeType::PipeWire))
        .unwrap();
    graph
        .add_port(
            Port::new(
                PortId(7),
                NodeId(3),
                "input_MONO",
                Direction::Sink,
                PortType::Audio,
            )
            .with_channel("MONO"),
        )
        .unwrap();
    let mut state = easy_state();
    state.ids.rebuild(&graph);

    // Nothing lines up by channel, so the drag pairs in order instead of
    // reporting that there is nothing to connect.
    let pairs = state.matching_port_pairs(&graph, NodeId(1), NodeId(3));

    assert_eq!(pairs, vec![(PortId(1), PortId(7))]);
}

#[test]
fn channel_matching_wins_over_port_order() {
    let mut graph = Graph::default();
    graph
        .add_node(Node::new(NodeId(1), "Capture", NodeType::PipeWire))
        .unwrap();
    graph
        .add_node(Node::new(NodeId(2), "Playback", NodeType::PipeWire))
        .unwrap();
    // The sink lists its right channel first.
    for (id, node, name, direction, channel) in [
        (1, NodeId(1), "capture_FL", Direction::Source, "FL"),
        (2, NodeId(1), "capture_FR", Direction::Source, "FR"),
        (3, NodeId(2), "playback_FR", Direction::Sink, "FR"),
        (4, NodeId(2), "playback_FL", Direction::Sink, "FL"),
    ] {
        graph
            .add_port(
                Port::new(PortId(id), node, name, direction, PortType::Audio).with_channel(channel),
            )
            .unwrap();
    }
    let mut state = easy_state();
    state.ids.rebuild(&graph);

    let pairs = state.matching_port_pairs(&graph, NodeId(1), NodeId(2));

    assert_eq!(pairs, vec![(PortId(1), PortId(4)), (PortId(2), PortId(3))]);
}

#[test]
fn topology_fingerprint_ignores_moves_but_not_links() {
    let mut graph = graph();
    let before = topology_fingerprint(&graph);
    graph.nodes.get_mut(&NodeId(1)).unwrap().position = [999.0, 999.0];
    assert_eq!(
        topology_fingerprint(&graph),
        before,
        "moving a card must not invalidate the cached auto-layout"
    );
    graph
        .add_node(Node::new(NodeId(3), "Extra", NodeType::PipeWire))
        .unwrap();
    assert_ne!(
        topology_fingerprint(&graph),
        before,
        "a topology change must invalidate the cached auto-layout"
    );
}

#[test]
fn repulsion_ignores_distant_cards() {
    // One colliding obstacle plus 200 far-away cards: the old Cartesian
    // solver built ~400x400 candidates here; the bounded solver only pushes
    // out of the one overlap and must agree with the two-card solution.
    let config = AppConfig::default();
    let mut state = UiGraphState::from_config(&config);
    let base = graph();
    let mut snapshot = state.snapshot(&base, &config);
    snapshot.nodes[0].position = [0.0, 0.0];
    snapshot.nodes[0].width = 100.0;
    snapshot.nodes[0].height = 100.0;
    snapshot.nodes[1].position = [100.0, 100.0];
    snapshot.nodes[1].width = 100.0;
    snapshot.nodes[1].height = 100.0;
    for extra in 0..200 {
        let mut far = snapshot.nodes[1].clone();
        far.node_id = NodeId(1000 + extra);
        far.id = 1000 + extra as i32;
        far.position = [10_000.0 + extra as f32 * 500.0, 10_000.0];
        snapshot.nodes.push(far);
    }
    let selected = BTreeSet::from([snapshot.nodes[0].node_id]);

    let resolved = resolve_drag_delta(&snapshot, &selected, [100.0, 100.0], true);

    assert_eq!(resolved, [100.0, -18.0]);
    let dragged = snapshot
        .nodes
        .iter()
        .filter(|node| selected.contains(&node.node_id))
        .collect::<Vec<_>>();
    let stationary = snapshot
        .nodes
        .iter()
        .filter(|node| !selected.contains(&node.node_id))
        .collect::<Vec<_>>();
    assert!(drag_is_clear(&dragged, &stationary, resolved));
}
