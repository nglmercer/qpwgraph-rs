use super::*;

/// Merging two graphs clones whole nodes -- ports vec included -- and then
/// re-adds every port. Listing an id twice gives the port two rows and two
/// pins on the card, and the phantom second pin steals the link that
/// belongs to the real one, so edges are drawn to the wrong place.
#[test]
fn re_adding_a_port_a_node_already_lists_does_not_duplicate_it() {
    let source = graph();
    let mut merged = Graph::default();
    for node in source.nodes.values().cloned() {
        merged.add_node(node).unwrap();
    }
    for port in source.ports.values().cloned() {
        merged.add_port(port).unwrap();
    }

    for node in merged.nodes.values() {
        let mut unique = node.ports.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(
            node.ports.len(),
            unique.len(),
            "node {:?} lists a port more than once: {:?}",
            node.name,
            node.ports
        );
    }
    assert_eq!(merged.nodes[&NodeId(1)].ports, vec![PortId(1)]);
    assert_eq!(merged.ports.len(), source.ports.len());
}

fn graph() -> Graph {
    let mut graph = Graph::default();
    graph
        .add_node(Node::new(NodeId(1), "Source", NodeType::PipeWire))
        .unwrap();
    graph
        .add_node(Node::new(NodeId(2), "Sink", NodeType::PipeWire))
        .unwrap();
    graph
        .add_port(Port::new(
            PortId(1),
            NodeId(1),
            "out",
            Direction::Source,
            PortType::Audio,
        ))
        .unwrap();
    graph
        .add_port(Port::new(
            PortId(2),
            NodeId(2),
            "in",
            Direction::Sink,
            PortType::Audio,
        ))
        .unwrap();
    graph
}

#[test]
fn validates_and_removes_links() {
    let mut graph = graph();
    graph.add_link(LinkId(1), PortId(1), PortId(2)).unwrap();
    assert_eq!(graph.links.len(), 1);
    graph.remove_link(LinkId(1)).unwrap();
    assert!(graph.links.is_empty());
}

#[test]
fn rejects_wrong_direction() {
    let mut graph = graph();
    let error = graph.add_link(LinkId(1), PortId(2), PortId(1)).unwrap_err();
    assert_eq!(error, GraphError::NotSource(PortId(2)));
}

#[test]
fn stable_serial_resolves_a_renamed_windows_endpoint() {
    let node_id = NodeId(encode_backend_id(BackendNamespace::WindowsAudio, 1));
    let port_id = PortId(encode_backend_id(BackendNamespace::WindowsAudio, 2));
    let mut graph = Graph::default();
    graph
        .add_node(
            Node::new(
                node_id,
                "Speakers (old name)",
                NodeType::WindowsAudioEndpoint,
            )
            .with_serial(0x1234),
        )
        .unwrap();
    graph
        .add_port(Port::new(
            port_id,
            node_id,
            "audio",
            Direction::Sink,
            PortType::Audio,
        ))
        .unwrap();

    let key = graph.port_key(port_id).unwrap();
    graph.nodes.get_mut(&node_id).unwrap().name = "Speakers (new name)".into();

    assert_eq!(graph.resolve_port_key(&key), Some(port_id));
}

fn application_graph(
    node_id: u64,
    port_id: u64,
    node_name: &str,
    application_id: &str,
    serial: u64,
    port_name: &str,
    channel: Option<&str>,
) -> Graph {
    let mut graph = Graph::default();
    graph
        .add_node(
            Node::new(NodeId(node_id), node_name, NodeType::PipeWire).with_identity(NodeIdentity {
                application_id: Some(application_id.into()),
                application_name: Some("Test application".into()),
                process_binary: Some("test-app".into()),
                node_name: node_name.into(),
                media_role: Some("music".into()),
                object_serial: Some(serial),
                ..NodeIdentity::default()
            }),
        )
        .unwrap();
    let port = Port::new(
        PortId(port_id),
        NodeId(node_id),
        port_name,
        Direction::Source,
        PortType::Audio,
    );
    graph
        .add_port(channel.map_or(port.clone(), |channel| port.with_channel(channel)))
        .unwrap();
    graph
}

#[test]
fn application_selector_survives_global_id_churn() {
    let old = application_graph(
        10,
        20,
        "Discord",
        "com.example.discord",
        100,
        "output",
        None,
    );
    let key = old.port_key(PortId(20)).unwrap();
    let current = application_graph(
        145,
        227,
        "Discord",
        "com.example.discord",
        800,
        "output",
        None,
    );

    assert_eq!(
        current.resolve_port_key_result(&key),
        EndpointResolution::Exact(PortId(227))
    );
}

#[test]
fn application_identity_survives_serial_churn_and_node_rename() {
    let old = application_graph(
        10,
        20,
        "Discord playback",
        "com.example.discord",
        100,
        "output",
        None,
    );
    let key = old.port_key(PortId(20)).unwrap();
    let current = application_graph(
        300,
        455,
        "Discord stream",
        "com.example.discord",
        801,
        "output",
        None,
    );

    assert_eq!(
        current.resolve_port_key_result(&key),
        EndpointResolution::Exact(PortId(455))
    );
}

#[test]
fn process_identity_can_follow_a_renamed_port_when_application_id_is_absent() {
    let mut old = application_graph(
        10,
        20,
        "Discord playback",
        "org.example.discord",
        100,
        "old-output",
        Some("FL"),
    );
    old.nodes
        .get_mut(&NodeId(10))
        .expect("old node exists")
        .identity
        .application_id = None;
    let key = old.port_key(PortId(20)).unwrap();

    let mut current = application_graph(
        300,
        455,
        "Discord recreated",
        "org.example.discord",
        801,
        "new-output",
        Some("FL"),
    );
    current
        .nodes
        .get_mut(&NodeId(300))
        .expect("current node exists")
        .identity
        .application_id = None;

    assert_eq!(
        current.resolve_port_key_result(&key),
        EndpointResolution::UniqueFallback(PortId(455))
    );
}

#[test]
fn same_named_applications_do_not_cross_match() {
    let mut graph = Graph::default();
    for (node_id, port_id, application_id) in [(1, 10, "app.one"), (2, 20, "app.two")] {
        let node = Node::new(NodeId(node_id), "Same name", NodeType::PipeWire).with_identity(
            NodeIdentity {
                application_id: Some(application_id.into()),
                node_name: "Same name".into(),
                ..NodeIdentity::default()
            },
        );
        graph.add_node(node).unwrap();
        graph
            .add_port(Port::new(
                PortId(port_id),
                NodeId(node_id),
                "output",
                Direction::Source,
                PortType::Audio,
            ))
            .unwrap();
    }
    let key = graph.port_key(PortId(10)).unwrap();

    assert_eq!(graph.resolve_port_key(&key), Some(PortId(10)));
}

#[test]
fn duplicate_same_application_streams_are_ambiguous() {
    let mut graph = Graph::default();
    for (node_id, port_id, serial) in [(1, 10, 1), (2, 20, 2)] {
        let node = Node::new(NodeId(node_id), "Browser stream", NodeType::PipeWire).with_identity(
            NodeIdentity {
                application_id: Some("org.example.browser".into()),
                node_name: "Browser stream".into(),
                media_role: Some("music".into()),
                object_serial: Some(serial),
                ..NodeIdentity::default()
            },
        );
        graph.add_node(node).unwrap();
        graph
            .add_port(Port::new(
                PortId(port_id),
                NodeId(node_id),
                "output",
                Direction::Source,
                PortType::Audio,
            ))
            .unwrap();
    }
    let mut selector = EndpointSelector {
        node_type: NodeType::PipeWire,
        identity: NodeIdentity {
            application_id: Some("org.example.browser".into()),
            node_name: "Browser stream".into(),
            media_role: Some("music".into()),
            ..NodeIdentity::default()
        },
        port_name: "output".into(),
        channel: None,
        direction: Direction::Source,
        port_type: PortType::Audio,
        match_mode: EndpointMatchMode::Instance,
    };

    assert_eq!(
        graph.resolve_endpoint(&selector),
        EndpointResolution::Ambiguous(vec![PortId(10), PortId(20)])
    );

    selector.match_mode = EndpointMatchMode::Application;
    assert_eq!(
        graph.resolve_endpoint(&selector),
        EndpointResolution::Ambiguous(vec![PortId(10), PortId(20)])
    );
}

#[test]
fn stereo_channel_selector_cannot_reverse_left_and_right() {
    let mut graph = application_graph(
        1,
        10,
        "Stereo app",
        "org.example.stereo",
        1,
        "output",
        Some("FL"),
    );
    graph
        .add_port(
            Port::new(
                PortId(11),
                NodeId(1),
                "output",
                Direction::Source,
                PortType::Audio,
            )
            .with_channel("FR"),
        )
        .unwrap();
    let left = graph.port_key(PortId(10)).unwrap();
    assert_eq!(graph.resolve_port_key(&left), Some(PortId(10)));
}

#[test]
fn application_mode_can_follow_a_changed_port_name_by_channel() {
    let old = application_graph(
        10,
        20,
        "Browser",
        "org.example.browser",
        100,
        "old-output",
        Some("FL"),
    );
    let old_key = old.port_key(PortId(20)).unwrap();
    let mut selector = old_key.selector();
    selector.match_mode = EndpointMatchMode::Application;
    let current = application_graph(
        300,
        455,
        "Browser recreated",
        "org.example.browser",
        800,
        "new-output",
        Some("FL"),
    );

    assert_eq!(
        current.resolve_endpoint(&selector),
        EndpointResolution::Exact(PortId(455))
    );
}

#[test]
fn legacy_name_key_still_resolves_without_numeric_identity() {
    let graph = application_graph(10, 20, "Legacy source", "", 0, "output", None);
    let key = PortKey {
        node_name: "Legacy source".into(),
        node_serial: None,
        node_type: NodeType::PipeWire,
        port_name: "output".into(),
        channel: None,
        direction: Direction::Source,
        port_type: PortType::Audio,
        identity: None,
        match_mode: EndpointMatchMode::Instance,
    };

    assert_eq!(
        graph.resolve_port_key_result(&key),
        EndpointResolution::UniqueFallback(PortId(20))
    );
}

/// Build a graph holding both relay virtual nodes with their current
/// role-prefixed ports, plus an unrelated device that also has `FL`/`FR`.
fn relay_graph() -> (Graph, Vec<(PortId, &'static str)>) {
    let mut graph = Graph::default();
    graph
        .add_node(Node::new(
            NodeId(1),
            RELAY_SOURCE_NODE_NAME,
            NodeType::PipeWire,
        ))
        .unwrap();
    graph
        .add_node(Node::new(
            NodeId(2),
            RELAY_SINK_NODE_NAME,
            NodeType::PipeWire,
        ))
        .unwrap();
    graph
        .add_node(Node::new(
            NodeId(3),
            "alsa_output.pci-0000_00",
            NodeType::PipeWire,
        ))
        .unwrap();

    let mut ports = Vec::new();
    for (id, node, name, channel, direction) in [
        (10u64, NodeId(1), "capture_FL", "FL", Direction::Source),
        (11, NodeId(1), "capture_FR", "FR", Direction::Source),
        (12, NodeId(2), "playback_FL", "FL", Direction::Sink),
        (13, NodeId(2), "playback_FR", "FR", Direction::Sink),
        // The unrelated card still uses bare channel port names.
        (14, NodeId(3), "FL", "FL", Direction::Sink),
        (15, NodeId(3), "FR", "FR", Direction::Sink),
    ] {
        graph
            .add_port(
                Port::new(PortId(id), node, name, direction, PortType::Audio).with_channel(channel),
            )
            .unwrap();
        ports.push((PortId(id), name));
    }
    (graph, ports)
}

/// A patchbay key exactly as an older file would have stored it: the
/// relay node name with the bare channel as the port name.
fn legacy_relay_key(node_name: &str, port_name: &str, direction: Direction) -> PortKey {
    PortKey {
        node_name: node_name.into(),
        node_serial: None,
        node_type: NodeType::PipeWire,
        port_name: port_name.into(),
        channel: Some(port_name.into()),
        direction,
        port_type: PortType::Audio,
        identity: None,
        match_mode: EndpointMatchMode::Instance,
    }
}

#[test]
fn current_relay_patchbay_keys_resolve() {
    let (graph, ports) = relay_graph();
    for (id, _) in &ports[..4] {
        let key = graph.port_key(*id).unwrap();
        assert_eq!(graph.resolve_port_key(&key), Some(*id));
    }
}

#[test]
fn legacy_relay_source_keys_resolve_to_the_capture_ports() {
    // Regression: renaming the relay filter ports to `capture_*` silently
    // orphaned every relay connection in a patchbay saved before it.
    let (graph, _) = relay_graph();
    assert_eq!(
        graph.resolve_port_key(&legacy_relay_key(
            RELAY_SOURCE_NODE_NAME,
            "FL",
            Direction::Source
        )),
        Some(PortId(10))
    );
    assert_eq!(
        graph.resolve_port_key(&legacy_relay_key(
            RELAY_SOURCE_NODE_NAME,
            "FR",
            Direction::Source
        )),
        Some(PortId(11))
    );
}

#[test]
fn legacy_relay_sink_keys_resolve_to_the_playback_ports() {
    let (graph, _) = relay_graph();
    assert_eq!(
        graph.resolve_port_key(&legacy_relay_key(
            RELAY_SINK_NODE_NAME,
            "FL",
            Direction::Sink
        )),
        Some(PortId(12))
    );
    assert_eq!(
        graph.resolve_port_key(&legacy_relay_key(
            RELAY_SINK_NODE_NAME,
            "FR",
            Direction::Sink
        )),
        Some(PortId(13))
    );
}

#[test]
fn unrelated_devices_keep_their_own_fl_fr_ports() {
    // The compatibility rewrite must not make a normal card's `FL` pin
    // resolve to a relay port, nor stop resolving to itself.
    let (graph, _) = relay_graph();
    let key = legacy_relay_key("alsa_output.pci-0000_00", "FL", Direction::Sink);
    assert_eq!(graph.resolve_port_key(&key), Some(PortId(14)));

    // A relay key in the direction that node has no ports in is not
    // rewritten into the other relay node either.
    let wrong_direction = legacy_relay_key(RELAY_SOURCE_NODE_NAME, "FL", Direction::Sink);
    assert_eq!(graph.resolve_port_key(&wrong_direction), None);
}

#[test]
fn saving_a_relay_port_still_writes_the_role_prefixed_name() {
    // The migration is read-only: new patchbays must keep the new names.
    let (graph, _) = relay_graph();
    assert_eq!(graph.port_key(PortId(10)).unwrap().port_name, "capture_FL");
    assert_eq!(graph.port_key(PortId(12)).unwrap().port_name, "playback_FL");
}

#[test]
fn default_layout_groups_media_and_direction() {
    let mut graph = Graph::default();
    for (id, name) in [(1, "Audio source"), (2, "Audio sink"), (3, "MIDI source")] {
        graph
            .add_node(Node::new(NodeId(id), name, NodeType::PipeWire))
            .unwrap();
    }
    graph
        .add_port(Port::new(
            PortId(10),
            NodeId(1),
            "out",
            Direction::Source,
            PortType::Audio,
        ))
        .unwrap();
    graph
        .add_port(Port::new(
            PortId(11),
            NodeId(2),
            "in",
            Direction::Sink,
            PortType::Audio,
        ))
        .unwrap();
    graph
        .add_port(Port::new(
            PortId(12),
            NodeId(3),
            "out",
            Direction::Source,
            PortType::MidiJack,
        ))
        .unwrap();

    let positions = graph.default_node_positions();
    assert!(positions[&NodeId(1)][0] < positions[&NodeId(2)][0]);
    assert!(positions[&NodeId(1)][1] < positions[&NodeId(3)][1]);
}

#[test]
fn default_layout_places_connected_hops_in_ordered_layers() {
    let mut graph = Graph::default();
    for (id, name) in [(1, "Source"), (2, "Mixer"), (3, "Sink")] {
        graph
            .add_node(Node::new(NodeId(id), name, NodeType::PipeWire))
            .unwrap();
    }
    for (id, node, name, direction) in [
        (10, 1, "out", Direction::Source),
        (11, 2, "in", Direction::Sink),
        (12, 2, "out", Direction::Source),
        (13, 3, "in", Direction::Sink),
    ] {
        graph
            .add_port(Port::new(
                PortId(id),
                NodeId(node),
                name,
                direction,
                PortType::Audio,
            ))
            .unwrap();
    }
    graph.add_link(LinkId(20), PortId(10), PortId(11)).unwrap();
    graph.add_link(LinkId(21), PortId(12), PortId(13)).unwrap();

    let positions = graph.default_node_positions();
    assert!(positions[&NodeId(1)][0] < positions[&NodeId(2)][0]);
    assert!(positions[&NodeId(2)][0] < positions[&NodeId(3)][0]);
    assert_eq!(positions, graph.default_node_positions());
}

#[test]
fn default_layout_aligns_edges_and_separates_disconnected_sources() {
    let mut graph = Graph::default();
    graph
        .add_node(Node::new(NodeId(1), "Microphone", NodeType::PipeWire))
        .unwrap();
    graph
        .add_node(
            Node::new(NodeId(2), "Application", NodeType::PipeWire).with_identity(NodeIdentity {
                application_id: Some("org.example.app".into()),
                node_name: "Application".into(),
                ..NodeIdentity::default()
            }),
        )
        .unwrap();
    // Deliberately reverse the output names relative to the source IDs:
    // alphabetical ordering would put the two edges in the wrong order.
    graph
        .add_node(Node::new(NodeId(3), "Z output", NodeType::PipeWire))
        .unwrap();
    graph
        .add_node(Node::new(NodeId(4), "A output", NodeType::PipeWire))
        .unwrap();
    graph
        .add_node(Node::new(
            NodeId(5),
            "Unconnected microphone",
            NodeType::PipeWire,
        ))
        .unwrap();
    for (id, node, name, direction) in [
        (10, 1, "out", Direction::Source),
        (11, 2, "out", Direction::Source),
        (12, 3, "in", Direction::Sink),
        (13, 4, "in", Direction::Sink),
        (14, 5, "out", Direction::Source),
    ] {
        graph
            .add_port(Port::new(
                PortId(id),
                NodeId(node),
                name,
                direction,
                PortType::Audio,
            ))
            .unwrap();
    }
    graph.add_link(LinkId(20), PortId(10), PortId(12)).unwrap();
    graph.add_link(LinkId(21), PortId(11), PortId(13)).unwrap();

    let positions = graph.default_node_positions();

    assert!(positions[&NodeId(1)][0] < positions[&NodeId(3)][0]);
    assert!(positions[&NodeId(1)][1] < positions[&NodeId(2)][1]);
    assert!(positions[&NodeId(2)][1] < positions[&NodeId(5)][1]);
    assert!(
        positions[&NodeId(3)][1] < positions[&NodeId(4)][1],
        "connected output order should follow the source order: {positions:?}"
    );
    assert_eq!(positions, graph.default_node_positions());
}

#[test]
fn backend_ids_round_trip_each_public_namespace() {
    for (namespace, backend) in [
        (BackendNamespace::PipeWire, BackendKind::PipeWire),
        (BackendNamespace::AlsaMidi, BackendKind::AlsaMidi),
        (BackendNamespace::WindowsAudio, BackendKind::WindowsAudio),
        (BackendNamespace::WindowsMidi, BackendKind::WindowsMidi),
        (BackendNamespace::Demo, BackendKind::Demo),
    ] {
        let id = encode_backend_id(namespace, 0x1234_5678);
        assert_eq!(decode_backend_namespace(id), namespace);
        assert_eq!(decode_backend_local_id(id), 0x1234_5678);
        assert_eq!(namespace.backend_kind(), Some(backend));
    }
}

#[test]
fn backend_helpers_classify_typed_graph_ids() {
    assert_eq!(
        backend_for_node(NodeId(encode_backend_id(BackendNamespace::PipeWire, 7,))),
        Some(BackendKind::PipeWire)
    );
    assert_eq!(
        backend_for_port(PortId(
            encode_backend_id(BackendNamespace::WindowsAudio, 8,)
        )),
        Some(BackendKind::WindowsAudio)
    );
    assert_eq!(
        backend_for_link(LinkId(encode_backend_id(BackendNamespace::AlsaMidi, 9,))),
        Some(BackendKind::AlsaMidi)
    );
    assert_eq!(backend_for_node(NodeId(42)), Some(BackendKind::PipeWire));
}

#[test]
fn legacy_alsa_high_bit_ids_still_decode() {
    let legacy = (1_u64 << 63) | 42;
    assert_eq!(decode_backend_namespace(legacy), BackendNamespace::AlsaMidi);
    assert_eq!(decode_backend_local_id(legacy), 42);
    assert_eq!(
        backend_for_port(PortId(legacy)),
        Some(BackendKind::AlsaMidi)
    );
}

fn video_graph() -> Graph {
    let mut graph = Graph::default();
    graph
        .add_node(Node::new(NodeId(1), "Screen Capture", NodeType::PipeWire))
        .unwrap();
    graph
        .add_node(Node::new(NodeId(2), "Grayscale Filter", NodeType::Effect))
        .unwrap();
    graph
        .add_node(Node::new(NodeId(3), "Video Sink", NodeType::PipeWire))
        .unwrap();
    graph
        .add_node(Node::new(NodeId(4), "Speakers", NodeType::PipeWire))
        .unwrap();
    graph
        .add_port(Port::new(
            PortId(10),
            NodeId(1),
            "video_out",
            Direction::Source,
            PortType::Video,
        ))
        .unwrap();
    graph
        .add_port(Port::new(
            PortId(20),
            NodeId(2),
            "video_in",
            Direction::Sink,
            PortType::Video,
        ))
        .unwrap();
    graph
        .add_port(Port::new(
            PortId(21),
            NodeId(2),
            "video_out",
            Direction::Source,
            PortType::Video,
        ))
        .unwrap();
    graph
        .add_port(Port::new(
            PortId(30),
            NodeId(3),
            "video_in",
            Direction::Sink,
            PortType::Video,
        ))
        .unwrap();
    graph
        .add_port(Port::new(
            PortId(40),
            NodeId(4),
            "playback_FL",
            Direction::Sink,
            PortType::Audio,
        ))
        .unwrap();
    graph
        .add_port(Port::new(
            PortId(11),
            NodeId(1),
            "audio_out",
            Direction::Source,
            PortType::Audio,
        ))
        .unwrap();
    graph
}

#[test]
fn video_source_links_to_video_sink_through_a_filter() {
    let mut graph = video_graph();
    graph.add_link(LinkId(100), PortId(10), PortId(20)).unwrap();
    graph.add_link(LinkId(101), PortId(21), PortId(30)).unwrap();
    assert_eq!(graph.links.len(), 2);
}

#[test]
fn audio_and_video_ports_never_link() {
    let mut graph = video_graph();
    assert_eq!(
        graph.add_link(LinkId(100), PortId(11), PortId(20)),
        Err(GraphError::IncompatiblePorts(PortId(11), PortId(20)))
    );
    assert_eq!(
        graph.add_link(LinkId(101), PortId(10), PortId(40)),
        Err(GraphError::IncompatiblePorts(PortId(10), PortId(40)))
    );
    assert_eq!(
        graph.add_link(LinkId(102), PortId(21), PortId(40)),
        Err(GraphError::IncompatiblePorts(PortId(21), PortId(40)))
    );
    assert!(graph.links.is_empty());
}

#[test]
fn video_links_still_enforce_direction_and_duplicates() {
    let mut graph = video_graph();
    assert_eq!(
        graph.add_link(LinkId(100), PortId(20), PortId(30)),
        Err(GraphError::NotSource(PortId(20)))
    );
    assert_eq!(
        graph.add_link(LinkId(100), PortId(10), PortId(21)),
        Err(GraphError::NotSink(PortId(21)))
    );
    graph.add_link(LinkId(100), PortId(10), PortId(20)).unwrap();
    assert_eq!(
        graph.add_link(LinkId(101), PortId(10), PortId(20)),
        Err(GraphError::DuplicateConnection(PortId(10), PortId(20)))
    );
}

#[test]
fn camera_nodes_are_detected_by_role_or_monitor_prefix() {
    // media.role=Camera is what PipeWire sets on camera nodes.
    let mut role = Node::new(NodeId(1), "some-camera", NodeType::PipeWire);
    role.identity.media_role = Some("Camera".into());
    assert!(role.is_camera());

    // Name prefixes cover daemons that don't set the role.
    let v4l2 = Node::new(NodeId(2), "v4l2_input.pci-0000_00_10.0", NodeType::PipeWire);
    assert!(v4l2.is_camera());
    let libcamera = Node::new(NodeId(3), "libcamera_input.ipu6", NodeType::PipeWire);
    assert!(libcamera.is_camera());

    // A bare Video/Source class catches cameras with unusual names.
    let mut classed = Node::new(NodeId(6), "HD WebCam", NodeType::PipeWire);
    classed.identity.media_class = "Video/Source".into();
    assert!(classed.is_camera());
    // Screencast streams share the video world but are not cameras.
    let mut stream = Node::new(NodeId(7), "spot", NodeType::PipeWire);
    stream.identity.media_class = "Stream/Output/Video".into();
    assert!(!stream.is_camera());

    // Ordinary nodes are not cameras.
    let plain = Node::new(NodeId(4), "alsa_output.usb", NodeType::PipeWire);
    assert!(!plain.is_camera());
    let mut roleless = Node::new(NodeId(5), "v4l2_output.sink", NodeType::PipeWire);
    roleless.identity.media_role = Some("DSP".into());
    assert!(!roleless.is_camera());
}
