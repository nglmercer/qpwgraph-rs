use super::selectors::selector_sidecar_path;
use pw_graph_core::{Direction, Graph, NodeIdentity, NodeType, PortId, PortKey, PortType};
use std::time::{Duration, Instant};

use super::*;
use pw_graph_backend::{
    video::{VideoDriver, VideoFilterRequest},
    BackendResult, EffectDriver, EffectNodeRequest, GraphDriver, InMemoryDriver,
};
use std::collections::BTreeMap;

struct ObservedDemo {
    inner: InMemoryDriver,
}

impl EffectDriver for ObservedDemo {}

impl GraphDriver for ObservedDemo {
    fn refresh(&mut self) -> BackendResult<Vec<pw_graph_core::Node>> {
        self.inner.refresh()
    }

    fn connect(&mut self, src: PortId, dst: PortId) -> BackendResult<pw_graph_core::Link> {
        self.inner.connect(src, dst)
    }

    fn disconnect(&mut self, link: pw_graph_core::LinkId) -> BackendResult<pw_graph_core::Link> {
        self.inner.disconnect(link)
    }

    fn is_link_mutable(&self, _link: pw_graph_core::LinkId) -> bool {
        false
    }

    fn graph(&self) -> &Graph {
        self.inner.graph()
    }
}

fn graph_with_named_audio_edge(
    output_node_type: NodeType,
    input_node_type: NodeType,
    node_offset: u64,
    port_offset: u64,
) -> Graph {
    let mut graph = Graph::default();
    graph
        .add_node(pw_graph_core::Node::new(
            pw_graph_core::NodeId(node_offset + 1),
            "Capture",
            output_node_type,
        ))
        .unwrap();
    graph
        .add_node(pw_graph_core::Node::new(
            pw_graph_core::NodeId(node_offset + 2),
            "Noise Gate (gate-1)",
            input_node_type,
        ))
        .unwrap();
    graph
        .add_port(pw_graph_core::Port::new(
            PortId(port_offset + 1),
            pw_graph_core::NodeId(node_offset + 1),
            "output",
            Direction::Source,
            PortType::Audio,
        ))
        .unwrap();
    graph
        .add_port(pw_graph_core::Port::new(
            PortId(port_offset + 2),
            pw_graph_core::NodeId(node_offset + 2),
            "input",
            Direction::Sink,
            PortType::Audio,
        ))
        .unwrap();
    graph
}

fn key(driver: &InMemoryDriver, port: u64) -> PortKey {
    driver
        .graph()
        .port_key(PortId(port))
        .expect("demo port exists")
}

#[test]
fn activates_and_is_idempotent() {
    let mut patchbay = Patchbay::new("demo");
    patchbay.add_connection(PortId(1), PortId(3), true);
    let mut driver = InMemoryDriver::demo();
    assert_eq!(
        patchbay
            .activate(&mut driver, false, false)
            .unwrap()
            .connected,
        1
    );
    assert_eq!(
        patchbay
            .activate(&mut driver, false, false)
            .unwrap()
            .already_present,
        1
    );
}

#[test]
fn a_failed_activation_restores_what_it_disconnected() {
    // The user-visible regression: activating a patchbay took working
    // links down, the desired route was refused, and the old links stayed
    // gone — so a failed activation silently killed audio.
    let mut driver = InMemoryDriver::demo();
    let existing = driver.connect(PortId(1), PortId(3)).unwrap();
    driver.fail_connect_of(PortId(2), PortId(3));

    let mut patchbay = Patchbay::default();
    patchbay.add_graph_connection(driver.graph(), PortId(2), PortId(3), false);

    let error = patchbay
        .activate(&mut driver, false, true)
        .expect_err("a fatal mutation failure must abort activation");
    assert!(matches!(error, PatchbayError::Backend(_)), "{error:?}");
    assert!(
        driver.graph().link(existing.id).is_some()
            || driver
                .graph()
                .find_link_by_keys(
                    &driver.graph().port_key(PortId(1)).unwrap(),
                    &driver.graph().port_key(PortId(3)).unwrap()
                )
                .is_some(),
        "the pre-existing route must survive a failed activation"
    );
}

#[test]
fn exclusive_activation_rolls_back_created_and_removed_links_as_one_transaction() {
    let mut driver = InMemoryDriver::demo();
    driver.connect(PortId(1), PortId(3)).unwrap(); // A
    driver.fail_connect_of(PortId(2), PortId(3)); // Y fails after X
    let mut patchbay = Patchbay::default();
    patchbay.add_connection(PortId(1), PortId(4), false); // X
    patchbay.add_connection(PortId(2), PortId(3), false); // Y

    let error = patchbay
        .activate(&mut driver, true, false)
        .expect_err("the later desired route fails");
    assert!(matches!(error, PatchbayError::Backend(_)), "{error:?}");
    assert!(driver
        .graph()
        .find_link_by_keys(&key(&driver, 1), &key(&driver, 3))
        .is_some());
    assert!(driver
        .graph()
        .find_link_by_keys(&key(&driver, 1), &key(&driver, 4))
        .is_none());
    assert!(driver
        .graph()
        .find_link_by_keys(&key(&driver, 2), &key(&driver, 3))
        .is_none());
}

#[test]
fn rollback_does_not_restore_a_route_created_and_removed_by_this_activation() {
    // X is created for the first desired route, then auto-disconnected
    // while attempting the second route. X never belonged to the graph
    // before activation and must not come back alongside the original C.
    let mut driver = InMemoryDriver::demo();
    driver.connect(PortId(1), PortId(4)).unwrap(); // C: original graph
    driver.fail_connect_of(PortId(2), PortId(3)); // Y fails after X

    let mut patchbay = Patchbay::default();
    patchbay.add_connection(PortId(1), PortId(3), false); // X
    patchbay.add_connection(PortId(2), PortId(3), false); // Y

    assert!(patchbay.activate(&mut driver, false, true).is_err());
    assert!(driver
        .graph()
        .find_link_by_keys(&key(&driver, 1), &key(&driver, 4))
        .is_some());
    assert!(driver
        .graph()
        .find_link_by_keys(&key(&driver, 1), &key(&driver, 3))
        .is_none());
    assert!(driver
        .graph()
        .find_link_by_keys(&key(&driver, 2), &key(&driver, 3))
        .is_none());
}

#[test]
fn auto_disconnect_failure_leaves_the_stale_route_intact() {
    let mut driver = InMemoryDriver::demo();
    driver.connect(PortId(1), PortId(3)).unwrap();
    driver.fail_disconnect_of_pair(PortId(1), PortId(3));
    let mut patchbay = Patchbay::default();
    patchbay.add_connection(PortId(2), PortId(3), false);

    let error = patchbay
        .activate(&mut driver, false, true)
        .expect_err("stale-route removal fails");
    assert!(matches!(error, PatchbayError::Backend(_)), "{error:?}");
    assert!(driver
        .graph()
        .find_link_by_keys(&key(&driver, 1), &key(&driver, 3))
        .is_some());
    assert!(driver
        .graph()
        .find_link_by_keys(&key(&driver, 2), &key(&driver, 3))
        .is_none());
}

#[test]
fn exclusive_removal_failure_leaves_the_original_graph_intact() {
    let mut driver = InMemoryDriver::demo();
    driver.connect(PortId(1), PortId(3)).unwrap();
    driver.fail_disconnect_of_pair(PortId(1), PortId(3));
    let mut patchbay = Patchbay::default();
    patchbay.add_connection(PortId(2), PortId(4), false);

    let error = patchbay
        .activate(&mut driver, true, false)
        .expect_err("exclusive removal fails");
    assert!(matches!(error, PatchbayError::Backend(_)), "{error:?}");
    assert!(driver
        .graph()
        .find_link_by_keys(&key(&driver, 1), &key(&driver, 3))
        .is_some());
    assert!(driver
        .graph()
        .find_link_by_keys(&key(&driver, 2), &key(&driver, 4))
        .is_none());
}

#[test]
fn activation_reports_created_and_removed_stranding_separately() {
    let mut driver = InMemoryDriver::demo();
    driver.connect(PortId(1), PortId(3)).unwrap(); // removed A
    driver.fail_connect_of(PortId(2), PortId(3)); // primary failure Y
    driver.fail_disconnect_of_pair(PortId(1), PortId(4)); // X cannot roll back
    driver.fail_connect_of(PortId(1), PortId(3)); // A cannot be restored
    let mut patchbay = Patchbay::default();
    patchbay.add_connection(PortId(1), PortId(4), false);
    patchbay.add_connection(PortId(2), PortId(3), false);

    let error = patchbay
        .activate(&mut driver, true, false)
        .expect_err("both rollback directions fail");
    match error {
        PatchbayError::ActivationNotRolledBack {
            created_links_left,
            removed_links_not_restored,
            ..
        } => {
            assert_eq!(created_links_left, 1);
            assert_eq!(removed_links_not_restored, 1);
        }
        other => panic!("expected separate rollback counts, got {other:?}"),
    }
    assert!(driver
        .graph()
        .find_link_by_keys(&key(&driver, 1), &key(&driver, 4))
        .is_some());
    assert!(driver
        .graph()
        .find_link_by_keys(&key(&driver, 1), &key(&driver, 3))
        .is_none());
}

#[test]
fn immutable_and_already_present_links_are_never_rolled_back_as_new() {
    let mut driver = InMemoryDriver::demo();
    let immutable = driver.connect(PortId(1), PortId(3)).unwrap();
    driver.mark_link_observed(immutable.id);
    driver.connect(PortId(1), PortId(4)).unwrap();
    driver.fail_connect_of(PortId(2), PortId(3));
    let mut patchbay = Patchbay::default();
    patchbay.add_connection(PortId(1), PortId(4), false);
    patchbay.add_connection(PortId(2), PortId(3), false);

    assert!(patchbay.activate(&mut driver, true, false).is_err());
    assert!(driver
        .graph()
        .find_link_by_keys(&key(&driver, 1), &key(&driver, 3))
        .is_some());
    assert!(driver
        .graph()
        .find_link_by_keys(&key(&driver, 1), &key(&driver, 4))
        .is_some());
}

#[test]
fn exclusive_activation_leaves_observed_links_alone() {
    // Observed relationships belong to the backend, not the patchbay.
    let mut driver = InMemoryDriver::demo();
    let observed = driver.connect(PortId(1), PortId(3)).unwrap();
    driver.mark_link_observed(observed.id);

    let patchbay = Patchbay::default();
    let report = patchbay.activate(&mut driver, true, false).unwrap();
    assert_eq!(report.disconnected, 0);
    assert!(driver.graph().link(observed.id).is_some());
}

#[test]
fn snapshot_driver_omits_observed_links() {
    let mut driver = ObservedDemo {
        inner: InMemoryDriver::demo(),
    };
    driver.connect(PortId(1), PortId(3)).unwrap();

    let mut patchbay = Patchbay::new("observed");
    patchbay.snapshot_driver(&driver, true);

    assert!(patchbay.connections.is_empty());
}

#[test]
fn qpwgraph_xml_round_trip_uses_names() {
    let mut patchbay = Patchbay::new("demo");
    let driver = InMemoryDriver::demo();
    patchbay.add_graph_connection(driver.graph(), PortId(1), PortId(3), true);
    let path = std::env::temp_dir().join(format!("qpwgraph-rs-{}.qpwgraph", std::process::id()));
    patchbay.save_to(&path).unwrap();
    let loaded = Patchbay::load_from(&path).unwrap();
    assert_eq!(loaded.connections.len(), 1);
    assert_eq!(loaded.connections[0].output_node, "Audio Capture");
    assert_eq!(loaded.connections[0].output_name, "capture_FL");
    assert!(loaded.connections[0].output_selector.is_some());
    assert!(loaded.connections[0].input_selector.is_some());
    let sidecar = selector_sidecar_path(&path);
    std::fs::remove_file(path).unwrap();
    let _ = std::fs::remove_file(sidecar);
}

#[test]
fn saving_a_patchbay_creates_missing_parent_directories() {
    let root =
        std::env::temp_dir().join(format!("pw-graph-patchbay-parent-{}", std::process::id()));
    let path = root.join("nested").join("connections.json");
    Patchbay::new("nested").save_to(&path).unwrap();
    assert!(path.is_file());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn effect_node_connections_restore_after_the_node_is_recreated() {
    let mut first = InMemoryDriver::demo();
    let effect = first
        .create_effect_node(EffectNodeRequest {
            instance_id: "persistent-gate".into(),
            effect_id: "builtin.noise-gate".into(),
            module_path: None,
            enabled: true,
            parameters: BTreeMap::new(),
            channel_policy: Default::default(),
            position: [260.0, 180.0],
        })
        .unwrap();
    first.connect(PortId(1), effect.input_port).unwrap();
    first.connect(effect.output_port, PortId(3)).unwrap();

    let mut saved = Patchbay::new("effects");
    saved.snapshot_graph(first.graph(), true);
    assert_eq!(saved.effect_connections().connections.len(), 2);

    let mut recreated = InMemoryDriver::demo();
    let recreated_effect = recreated
        .create_effect_node(EffectNodeRequest {
            instance_id: "persistent-gate".into(),
            effect_id: "builtin.noise-gate".into(),
            module_path: None,
            enabled: true,
            parameters: BTreeMap::new(),
            channel_policy: Default::default(),
            position: [260.0, 180.0],
        })
        .unwrap();
    let report = saved.activate(&mut recreated, false, false).unwrap();

    assert_eq!(report.connected, 2);
    assert!(recreated.graph().links.values().any(
        |link| link.output_port == PortId(1) && link.input_port == recreated_effect.input_port
    ));
    assert!(recreated.graph().links.values().any(|link| link.output_port
        == recreated_effect.output_port
        && link.input_port == PortId(3)));
}

#[test]
fn activation_prefers_names_when_pipewire_ids_change() {
    let mut saved = Patchbay::new("demo");
    let original = InMemoryDriver::demo();
    saved.add_graph_connection(original.graph(), PortId(1), PortId(3), true);

    let mut graph = Graph::default();
    graph
        .add_node(pw_graph_core::Node::new(
            pw_graph_core::NodeId(101),
            "Audio Capture",
            NodeType::PipeWire,
        ))
        .unwrap();
    graph
        .add_node(pw_graph_core::Node::new(
            pw_graph_core::NodeId(102),
            "Audio Playback",
            NodeType::PipeWire,
        ))
        .unwrap();
    graph
        .add_port(pw_graph_core::Port::new(
            PortId(201),
            pw_graph_core::NodeId(101),
            "capture_FL",
            Direction::Source,
            PortType::Audio,
        ))
        .unwrap();
    graph
        .add_port(pw_graph_core::Port::new(
            PortId(203),
            pw_graph_core::NodeId(102),
            "playback_FL",
            Direction::Sink,
            PortType::Audio,
        ))
        .unwrap();
    let mut current = InMemoryDriver::new(graph);

    let report = saved.activate(&mut current, false, false).unwrap();
    assert_eq!(report.connected, 1);
    assert!(current
        .graph()
        .links
        .values()
        .any(|link| link.output_port == PortId(201) && link.input_port == PortId(203)));
}

#[test]
fn restores_effect_connections_with_independent_endpoint_types() {
    for (output_node_type, input_node_type) in [
        (NodeType::PipeWire, NodeType::Effect),
        (NodeType::Effect, NodeType::PipeWire),
    ] {
        let original = graph_with_named_audio_edge(output_node_type, input_node_type, 10, 20);
        let mut patchbay = Patchbay::new("effects");
        patchbay.add_graph_connection(&original, PortId(21), PortId(22), true);

        let connection = &patchbay.connections[0];
        assert_eq!(connection.node_type, output_node_type);
        assert_eq!(connection.output_node_type, Some(output_node_type));
        assert_eq!(connection.input_node_type, Some(input_node_type));

        let mut current = InMemoryDriver::new(graph_with_named_audio_edge(
            output_node_type,
            input_node_type,
            100,
            200,
        ));
        let report = patchbay.activate(&mut current, false, false).unwrap();
        assert_eq!(report.connected, 1);
        assert!(current
            .graph()
            .links
            .values()
            .any(|link| { link.output_port == PortId(201) && link.input_port == PortId(202) }));
    }
}

#[test]
fn legacy_single_type_rule_can_restore_an_effect_endpoint() {
    let mut patchbay = Patchbay::new("legacy-effects");
    patchbay.connections.push(PatchConnection {
        node_type: NodeType::PipeWire,
        port_type: PortType::Audio,
        output_node: "Capture".into(),
        output_name: "output".into(),
        input_node: "Noise Gate (gate-1)".into(),
        input_name: "input".into(),
        ..PatchConnection::default()
    });

    let mut current = InMemoryDriver::new(graph_with_named_audio_edge(
        NodeType::PipeWire,
        NodeType::Effect,
        100,
        200,
    ));
    let report = patchbay.activate(&mut current, false, false).unwrap();
    assert_eq!(report.connected, 1);
    assert_eq!(report.failed, Vec::<String>::new());
}

#[test]
fn qpwgraph_xml_round_trip_preserves_effect_endpoint_types() {
    let original = graph_with_named_audio_edge(NodeType::Effect, NodeType::PipeWire, 10, 20);
    let mut patchbay = Patchbay::new("effects");
    patchbay.add_graph_connection(&original, PortId(21), PortId(22), true);

    let xml = patchbay.to_xml().unwrap();
    assert!(!xml.contains("output-node-type"));
    assert!(!xml.contains("input-node-type"));

    let path = std::env::temp_dir().join(format!(
        "qpwgraph-rs-effect-types-{}.qpwgraph",
        std::process::id()
    ));
    patchbay.save_to(&path).unwrap();
    let loaded = Patchbay::load_from(&path).unwrap();
    let connection = &loaded.connections[0];
    assert_eq!(connection.effective_output_node_type(), NodeType::Effect);
    assert_eq!(connection.effective_input_node_type(), NodeType::PipeWire);

    let same_type = graph_with_named_audio_edge(NodeType::Effect, NodeType::Effect, 30, 40);
    let mut same_type_patchbay = Patchbay::new("effects");
    same_type_patchbay.add_graph_connection(&same_type, PortId(41), PortId(42), true);
    let same_type_xml = same_type_patchbay.to_xml().unwrap();
    assert!(!same_type_xml.contains("input-node-type"));
    let same_type_path = std::env::temp_dir().join(format!(
        "qpwgraph-rs-effect-types-same-{}.qpwgraph",
        std::process::id()
    ));
    same_type_patchbay.save_to(&same_type_path).unwrap();
    let same_type_loaded = Patchbay::load_from(&same_type_path).unwrap();
    assert_eq!(
        same_type_loaded.connections[0].output_node_type,
        Some(NodeType::Effect)
    );
    assert_eq!(
        same_type_loaded.connections[0].input_node_type,
        Some(NodeType::Effect)
    );
    let _ = std::fs::remove_file(path.clone());
    let _ = std::fs::remove_file(selector_sidecar_path(&path));
    let _ = std::fs::remove_file(same_type_path.clone());
    let _ = std::fs::remove_file(selector_sidecar_path(&same_type_path));
}

#[test]
fn json_round_trip_preserves_effect_endpoint_types() {
    let original = graph_with_named_audio_edge(NodeType::PipeWire, NodeType::Effect, 10, 20);
    let mut patchbay = Patchbay::new("effects");
    patchbay.add_graph_connection(&original, PortId(21), PortId(22), true);

    let loaded: Patchbay =
        serde_json::from_str(&serde_json::to_string(&patchbay).unwrap()).unwrap();
    let connection = &loaded.connections[0];
    assert_eq!(connection.output_node_type, Some(NodeType::PipeWire));
    assert_eq!(connection.input_node_type, Some(NodeType::Effect));

    let mut current = InMemoryDriver::new(graph_with_named_audio_edge(
        NodeType::PipeWire,
        NodeType::Effect,
        100,
        200,
    ));
    assert_eq!(
        loaded
            .activate(&mut current, false, false)
            .unwrap()
            .connected,
        1
    );
}

#[test]
fn legacy_json_without_endpoint_types_still_loads() {
    let patchbay: Patchbay = serde_json::from_str(
        r#"{
            "version": 1,
            "name": "legacy",
            "connections": [{
                "output_port": 1,
                "input_port": 3,
                "pinned": true,
                "node_type": "PipeWire",
                "port_type": "Audio",
                "output_node": "Audio Capture",
                "output_name": "capture_FL",
                "input_node": "Audio Playback",
                "input_name": "playback_FL"
            }]
        }"#,
    )
    .unwrap();
    let connection = &patchbay.connections[0];
    assert_eq!(connection.output_node_type, None);
    assert_eq!(connection.input_node_type, None);
    assert_eq!(connection.effective_output_node_type(), NodeType::PipeWire);
    assert_eq!(connection.effective_input_node_type(), NodeType::PipeWire);
}

/// A graph holding both relay virtual nodes with their current
/// role-prefixed ports, plus a normal card that still names its ports
/// with the bare channel.
fn relay_graph() -> Graph {
    let mut graph = Graph::default();
    for (id, name) in [
        (1u64, pw_graph_core::RELAY_SOURCE_NODE_NAME),
        (2, pw_graph_core::RELAY_SINK_NODE_NAME),
        (3, "alsa_output.pci-0000_00"),
    ] {
        graph
            .add_node(pw_graph_core::Node::new(
                pw_graph_core::NodeId(id),
                name,
                NodeType::PipeWire,
            ))
            .unwrap();
    }
    for (id, node, name, direction) in [
        (10u64, 1u64, "capture_FL", Direction::Source),
        (11, 1, "capture_FR", Direction::Source),
        (12, 2, "playback_FL", Direction::Sink),
        (13, 2, "playback_FR", Direction::Sink),
        (14, 3, "FL", Direction::Sink),
        (15, 3, "FR", Direction::Sink),
    ] {
        graph
            .add_port(pw_graph_core::Port::new(
                PortId(id),
                pw_graph_core::NodeId(node),
                name,
                direction,
                PortType::Audio,
            ))
            .unwrap();
    }
    graph
}

fn relay_patchbay(output_name: &str, input_node: &str, input_name: &str) -> Patchbay {
    serde_json::from_str(&format!(
        r#"{{
            "version": 1,
            "name": "legacy relay",
            "connections": [{{
                "output_port": 0,
                "input_port": 0,
                "pinned": true,
                "port_type": "Audio",
                "output_node_type": "PipeWire",
                "input_node_type": "PipeWire",
                "output_node": "{}",
                "output_name": "{output_name}",
                "input_node": "{input_node}",
                "input_name": "{input_name}"
            }}]
        }}"#,
        pw_graph_core::RELAY_SOURCE_NODE_NAME
    ))
    .unwrap()
}

#[test]
fn a_patchbay_saved_with_the_current_relay_port_names_activates() {
    let patchbay = relay_patchbay(
        "capture_FL",
        pw_graph_core::RELAY_SINK_NODE_NAME,
        "playback_FL",
    );
    let mut driver = InMemoryDriver::new(relay_graph());
    assert_eq!(
        patchbay
            .activate(&mut driver, false, false)
            .unwrap()
            .connected,
        1
    );
}

#[test]
fn a_patchbay_saved_before_the_relay_port_rename_still_activates() {
    // Regression: relay ports were renamed `FL` -> `capture_FL` /
    // `playback_FL` to make the canvas group them as a stereo pair.
    // Without a compatibility rewrite every relay connection in an
    // already-saved patchbay silently stopped reconnecting.
    let patchbay = relay_patchbay("FL", pw_graph_core::RELAY_SINK_NODE_NAME, "FL");
    let mut driver = InMemoryDriver::new(relay_graph());
    let report = patchbay.activate(&mut driver, false, false).unwrap();
    assert_eq!(report.connected, 1);

    let link = driver
        .graph()
        .links
        .values()
        .find(|link| link.output_port == PortId(10) && link.input_port == PortId(12));
    assert!(
        link.is_some(),
        "legacy relay FL keys must land on the role-prefixed ports"
    );
}

#[test]
fn a_legacy_fl_rule_for_an_ordinary_card_is_not_rewritten() {
    // The rewrite is scoped to the relay nodes: a normal card whose ports
    // really are named `FL` must still resolve to its own pins.
    let patchbay = relay_patchbay("FL", "alsa_output.pci-0000_00", "FL");
    let mut driver = InMemoryDriver::new(relay_graph());
    assert_eq!(
        patchbay
            .activate(&mut driver, false, false)
            .unwrap()
            .connected,
        1
    );
    assert!(driver
        .graph()
        .links
        .values()
        .any(|link| link.output_port == PortId(10) && link.input_port == PortId(14)));
}

#[test]
fn removing_an_effect_removes_all_rules_touching_that_node() {
    let graph = graph_with_named_audio_edge(NodeType::PipeWire, NodeType::Effect, 10, 20);
    let mut patchbay = Patchbay::new("effects");
    patchbay.add_graph_connection(&graph, PortId(21), PortId(22), true);
    patchbay.connections.push(PatchConnection {
        output_node: "Other".into(),
        output_name: "output".into(),
        input_node: "Sink".into(),
        input_name: "input".into(),
        ..PatchConnection::default()
    });

    assert!(patchbay.remove_connections_for_node("Noise Gate (gate-1)"));
    assert_eq!(patchbay.connections.len(), 1);
    assert_eq!(patchbay.connections[0].output_node, "Other");
    assert!(!patchbay.remove_connections_for_node("Noise Gate (gate-1)"));
}

fn dynamic_application_graph(
    application_node_id: u64,
    application_port_id: u64,
    serial: u64,
    application_name: &str,
    sink_node_id: u64,
    sink_port_id: u64,
) -> Graph {
    let mut graph = Graph::default();
    graph
        .add_node(
            pw_graph_core::Node::new(
                pw_graph_core::NodeId(application_node_id),
                application_name,
                NodeType::PipeWire,
            )
            .with_identity(NodeIdentity {
                application_id: Some("org.example.browser".into()),
                application_name: Some("Browser".into()),
                process_binary: Some("browser".into()),
                node_name: application_name.into(),
                media_role: Some("music".into()),
                object_serial: Some(serial),
                ..NodeIdentity::default()
            }),
        )
        .unwrap();
    graph
        .add_node(pw_graph_core::Node::new(
            pw_graph_core::NodeId(sink_node_id),
            "Built-in sink",
            NodeType::PipeWire,
        ))
        .unwrap();
    graph
        .add_port(
            pw_graph_core::Port::new(
                PortId(application_port_id),
                pw_graph_core::NodeId(application_node_id),
                "output",
                Direction::Source,
                PortType::Audio,
            )
            .with_channel("FL"),
        )
        .unwrap();
    graph
        .add_port(
            pw_graph_core::Port::new(
                PortId(sink_port_id),
                pw_graph_core::NodeId(sink_node_id),
                "input",
                Direction::Sink,
                PortType::Audio,
            )
            .with_channel("FL"),
        )
        .unwrap();
    graph
}

fn reconcile_now(
    reconciler: &mut PatchbayReconciler,
    patchbay: &Patchbay,
    driver: &mut InMemoryDriver,
    now: Instant,
    exclusive: bool,
    auto_disconnect: bool,
) -> ReconcileReport {
    reconciler.schedule_now(now);
    reconciler
        .reconcile_if_due(patchbay, driver, exclusive, auto_disconnect, now, 1)
        .unwrap()
        .expect("scheduled reconciliation should run")
}

#[test]
fn reconciler_restores_a_recreated_application_stream() {
    let old = dynamic_application_graph(10, 20, 100, "Browser stream", 30, 40);
    let mut patchbay = Patchbay::new("dynamic");
    patchbay.add_graph_connection(&old, PortId(20), PortId(40), true);
    let mut driver = InMemoryDriver::new(old);
    driver.connect(PortId(20), PortId(40)).unwrap();

    let recreated = dynamic_application_graph(300, 455, 800, "Browser stream", 600, 655);
    driver.replace_graph(recreated);
    let now = Instant::now();
    let mut reconciler = PatchbayReconciler::new();
    let report = reconcile_now(&mut reconciler, &patchbay, &mut driver, now, false, false);

    assert_eq!(report.connected, 1);
    assert_eq!(report.rules[0].status, ReconcileStatus::Satisfied);
    assert!(driver
        .graph()
        .links
        .values()
        .any(|link| link.output_port == PortId(455) && link.input_port == PortId(655)));
}

#[test]
fn reconciler_coalesces_registry_bursts_and_never_duplicates_a_route() {
    let graph = dynamic_application_graph(10, 20, 100, "Browser stream", 30, 40);
    let mut patchbay = Patchbay::new("dynamic");
    patchbay.add_graph_connection(&graph, PortId(20), PortId(40), true);
    let mut driver = InMemoryDriver::new(graph);
    let now = Instant::now();
    let mut reconciler = PatchbayReconciler::new();
    for offset in [0, 20, 40, 60] {
        reconciler.mark_dirty(now + Duration::from_millis(offset));
    }
    assert!(reconciler
        .reconcile_if_due(
            &patchbay,
            &mut driver,
            false,
            false,
            now + Duration::from_millis(99),
            4,
        )
        .unwrap()
        .is_none());
    let report = reconciler
        .reconcile_if_due(
            &patchbay,
            &mut driver,
            false,
            false,
            now + Duration::from_millis(161),
            4,
        )
        .unwrap()
        .expect("one debounced pass should run");
    assert_eq!(report.connected, 1);
    assert!(reconciler
        .reconcile_if_due(
            &patchbay,
            &mut driver,
            false,
            false,
            now + Duration::from_millis(161),
            4,
        )
        .unwrap()
        .is_none());
    assert_eq!(driver.graph().links.len(), 1);
}

#[test]
fn missing_endpoint_stays_pending_without_mutating_the_graph() {
    let old = dynamic_application_graph(10, 20, 100, "Browser stream", 30, 40);
    let mut patchbay = Patchbay::new("dynamic");
    patchbay.add_graph_connection(&old, PortId(20), PortId(40), true);
    let mut current = old.clone();
    current.ports.remove(&PortId(20));
    current
        .nodes
        .get_mut(&pw_graph_core::NodeId(10))
        .unwrap()
        .ports
        .clear();
    let mut driver = InMemoryDriver::new(current);
    let mut reconciler = PatchbayReconciler::new();
    let report = reconcile_now(
        &mut reconciler,
        &patchbay,
        &mut driver,
        Instant::now(),
        false,
        false,
    );

    assert_eq!(report.rules[0].status, ReconcileStatus::WaitingForEndpoint);
    assert!(driver.graph().links.is_empty());
    assert_eq!(patchbay.connections.len(), 1);
}

#[test]
fn node_then_ports_is_retried_after_the_next_graph_generation() {
    let old = dynamic_application_graph(10, 20, 100, "Browser stream", 30, 40);
    let mut patchbay = Patchbay::new("dynamic");
    patchbay.add_graph_connection(&old, PortId(20), PortId(40), true);
    let mut partial = old.clone();
    partial.ports.remove(&PortId(20));
    partial
        .nodes
        .get_mut(&pw_graph_core::NodeId(10))
        .unwrap()
        .ports
        .clear();
    let mut driver = InMemoryDriver::new(partial);
    let mut reconciler = PatchbayReconciler::new();
    let first = reconcile_now(
        &mut reconciler,
        &patchbay,
        &mut driver,
        Instant::now(),
        false,
        false,
    );
    assert_eq!(first.rules[0].status, ReconcileStatus::WaitingForEndpoint);

    driver.replace_graph(dynamic_application_graph(
        300,
        455,
        800,
        "Browser stream",
        600,
        655,
    ));
    let second = reconcile_now(
        &mut reconciler,
        &patchbay,
        &mut driver,
        Instant::now(),
        false,
        false,
    );
    assert_eq!(second.connected, 1);
}

#[test]
fn equal_application_candidates_are_reported_ambiguous() {
    let old = dynamic_application_graph(10, 20, 100, "Browser stream", 30, 40);
    let mut patchbay = Patchbay::new("dynamic");
    patchbay.add_graph_connection(&old, PortId(20), PortId(40), true);
    let mut current = dynamic_application_graph(300, 455, 800, "Browser stream", 600, 655);
    let second = dynamic_application_graph(301, 456, 801, "Browser stream", 600, 655);
    current.nodes.insert(
        pw_graph_core::NodeId(301),
        second.nodes[&pw_graph_core::NodeId(301)].clone(),
    );
    current
        .ports
        .insert(PortId(456), second.ports[&PortId(456)].clone());
    let mut driver = InMemoryDriver::new(current);
    let mut reconciler = PatchbayReconciler::new();
    let report = reconcile_now(
        &mut reconciler,
        &patchbay,
        &mut driver,
        Instant::now(),
        false,
        false,
    );

    assert_eq!(report.rules[0].status, ReconcileStatus::Ambiguous);
    assert!(driver.graph().links.is_empty());
}

#[test]
fn transient_backend_failure_uses_bounded_exponential_retry() {
    let graph = dynamic_application_graph(10, 20, 100, "Browser stream", 30, 40);
    let mut patchbay = Patchbay::new("dynamic");
    patchbay.add_graph_connection(&graph, PortId(20), PortId(40), true);
    let mut driver = InMemoryDriver::new(graph);
    driver.fail_connect_of(PortId(20), PortId(40));
    let mut reconciler = PatchbayReconciler::new();
    let mut now = Instant::now();
    let mut final_status = None;
    reconciler.schedule_now(now);
    for attempt in 0..6 {
        let report = reconciler
            .reconcile_if_due(&patchbay, &mut driver, false, false, now, 1)
            .unwrap()
            .expect("the next retry should be due");
        final_status = report.rules.first().map(|rule| rule.status);
        if attempt < 5 {
            now += Duration::from_millis(100_u64 << (attempt + 1));
        }
    }
    assert_eq!(final_status, Some(ReconcileStatus::Failed));
    assert!(reconciler
        .reconcile_if_due(
            &patchbay,
            &mut driver,
            false,
            false,
            now + Duration::from_secs(60),
            1,
        )
        .unwrap()
        .is_none());
}

#[test]
fn deleting_a_saved_rule_prevents_reconciliation_from_recreating_it() {
    let graph = dynamic_application_graph(10, 20, 100, "Browser stream", 30, 40);
    let output = graph.port_key(PortId(20)).unwrap();
    let input = graph.port_key(PortId(40)).unwrap();
    let mut patchbay = Patchbay::new("dynamic");
    patchbay.add_graph_connection(&graph, PortId(20), PortId(40), true);
    let mut driver = InMemoryDriver::new(graph);
    let mut reconciler = PatchbayReconciler::new();
    reconcile_now(
        &mut reconciler,
        &patchbay,
        &mut driver,
        Instant::now(),
        false,
        false,
    );
    let link = driver.graph().links.values().next().unwrap().id;
    driver.disconnect(link).unwrap();
    assert!(patchbay.remove_stable_connection(&output, &input));

    let report = reconcile_now(
        &mut reconciler,
        &patchbay,
        &mut driver,
        Instant::now(),
        false,
        false,
    );
    assert!(report.rules.is_empty());
    assert!(driver.graph().links.is_empty());
}

#[test]
fn same_named_application_rules_remain_distinct_and_delete_only_one() {
    let first = dynamic_application_graph(10, 20, 100, "Browser stream", 30, 40);
    let mut second = dynamic_application_graph(11, 21, 101, "Browser stream", 30, 40);
    second
        .nodes
        .get_mut(&pw_graph_core::NodeId(11))
        .unwrap()
        .identity
        .application_id = Some("org.example.other-browser".into());
    let mut graph = first.clone();
    graph.nodes.insert(
        pw_graph_core::NodeId(11),
        second.nodes[&pw_graph_core::NodeId(11)].clone(),
    );
    graph
        .ports
        .insert(PortId(21), second.ports[&PortId(21)].clone());

    let mut patchbay = Patchbay::new("same-name");
    patchbay.add_graph_connection(&graph, PortId(20), PortId(40), true);
    patchbay.add_graph_connection(&graph, PortId(21), PortId(40), true);
    assert_eq!(patchbay.connections.len(), 2);

    let first_output = graph.port_key(PortId(20)).unwrap();
    let input = graph.port_key(PortId(40)).unwrap();
    assert!(patchbay.remove_stable_connection(&first_output, &input));
    assert_eq!(patchbay.connections.len(), 1);
    assert_eq!(
        patchbay.connections[0]
            .output_selector
            .as_ref()
            .and_then(|selector| selector.identity.application_id.as_deref()),
        Some("org.example.other-browser")
    );
    assert_eq!(patchbay.connections[0].output_port, PortId(21));
}

#[test]
fn exclusive_reconciliation_settles_without_oscillation() {
    let graph = dynamic_application_graph(10, 20, 100, "Browser stream", 30, 40);
    let mut extra = graph.clone();
    extra
        .add_node(pw_graph_core::Node::new(
            pw_graph_core::NodeId(50),
            "Other source",
            NodeType::PipeWire,
        ))
        .unwrap();
    extra
        .add_node(pw_graph_core::Node::new(
            pw_graph_core::NodeId(51),
            "Other sink",
            NodeType::PipeWire,
        ))
        .unwrap();
    extra
        .add_port(pw_graph_core::Port::new(
            PortId(60),
            pw_graph_core::NodeId(50),
            "output",
            Direction::Source,
            PortType::Audio,
        ))
        .unwrap();
    extra
        .add_port(pw_graph_core::Port::new(
            PortId(61),
            pw_graph_core::NodeId(51),
            "input",
            Direction::Sink,
            PortType::Audio,
        ))
        .unwrap();
    let mut patchbay = Patchbay::new("exclusive");
    patchbay.add_graph_connection(&graph, PortId(20), PortId(40), true);
    let mut driver = InMemoryDriver::new(extra);
    driver.connect(PortId(20), PortId(40)).unwrap();
    driver.connect(PortId(60), PortId(61)).unwrap();
    let mut reconciler = PatchbayReconciler::new();
    let first = reconcile_now(
        &mut reconciler,
        &patchbay,
        &mut driver,
        Instant::now(),
        true,
        false,
    );
    assert_eq!(first.disconnected, 1);
    assert_eq!(driver.graph().links.len(), 1);
    let second = reconcile_now(
        &mut reconciler,
        &patchbay,
        &mut driver,
        Instant::now(),
        true,
        false,
    );
    assert_eq!(second.disconnected, 0);
    assert_eq!(driver.graph().links.len(), 1);
}

#[test]
fn one_shot_exclusive_activation_does_not_remove_links_for_waiting_rules() {
    let old = dynamic_application_graph(10, 20, 100, "Browser stream", 30, 40);
    let mut patchbay = Patchbay::new("exclusive-waiting");
    patchbay.add_graph_connection(&old, PortId(20), PortId(40), true);

    let mut current = old.clone();
    current.nodes.remove(&pw_graph_core::NodeId(10));
    current.ports.remove(&PortId(20));
    current
        .nodes
        .get_mut(&pw_graph_core::NodeId(30))
        .unwrap()
        .ports
        .clear();
    current
        .add_node(pw_graph_core::Node::new(
            pw_graph_core::NodeId(50),
            "Other source",
            NodeType::PipeWire,
        ))
        .unwrap();
    current
        .add_node(pw_graph_core::Node::new(
            pw_graph_core::NodeId(51),
            "Other sink",
            NodeType::PipeWire,
        ))
        .unwrap();
    current
        .add_port(pw_graph_core::Port::new(
            PortId(60),
            pw_graph_core::NodeId(50),
            "output",
            Direction::Source,
            PortType::Audio,
        ))
        .unwrap();
    current
        .add_port(pw_graph_core::Port::new(
            PortId(61),
            pw_graph_core::NodeId(51),
            "input",
            Direction::Sink,
            PortType::Audio,
        ))
        .unwrap();
    let mut driver = InMemoryDriver::new(current);
    driver.connect(PortId(60), PortId(61)).unwrap();

    let report = patchbay.activate(&mut driver, true, false).unwrap();
    assert_eq!(report.disconnected, 0);
    assert_eq!(report.waiting.len(), 1);
    assert!(driver
        .graph()
        .find_link_by_keys(&key(&driver, 60), &key(&driver, 61))
        .is_some());
}

#[test]
fn repeated_application_recreation_does_not_accumulate_routes_or_stale_ids() {
    let first = dynamic_application_graph(10, 20, 100, "Browser stream", 30, 40);
    let mut patchbay = Patchbay::new("dynamic");
    patchbay.add_graph_connection(&first, PortId(20), PortId(40), true);
    let mut driver = InMemoryDriver::new(first);
    let mut reconciler = PatchbayReconciler::new();
    let mut now = Instant::now();
    for generation in 0..100_u64 {
        let base = 1000 + generation * 4;
        driver.replace_graph(dynamic_application_graph(
            base,
            base + 1,
            10_000 + generation,
            "Browser stream",
            base + 2,
            base + 3,
        ));
        reconcile_now(&mut reconciler, &patchbay, &mut driver, now, false, false);
        assert_eq!(driver.graph().links.len(), 1);
        assert_eq!(patchbay.connections.len(), 1);
        now += Duration::from_millis(1);
    }
}

#[test]
fn video_links_round_trip_through_xml_with_video_types() {
    let mut driver = InMemoryDriver::demo();
    let first = driver
        .create_video_filter(VideoFilterRequest {
            instance_id: "persist-a".into(),
            filter_id: "passthrough".into(),
            params: pw_graph_video::filters::FilterParams::default(),
            position: [0.0, 0.0],
        })
        .unwrap();
    let second = driver
        .create_video_filter(VideoFilterRequest {
            instance_id: "persist-b".into(),
            filter_id: "grayscale".into(),
            params: pw_graph_video::filters::FilterParams::default(),
            position: [0.0, 0.0],
        })
        .unwrap();
    driver
        .connect(first.output_port, second.input_port)
        .unwrap();

    let mut patchbay = Patchbay::new("video-test");
    patchbay.add_graph_connection(driver.graph(), first.output_port, second.input_port, false);
    assert_eq!(patchbay.connections.len(), 1);
    let xml = patchbay.to_xml().unwrap();
    assert!(xml.contains("pipewire-video"), "{xml}");

    let restored = Patchbay::from_xml(&xml).unwrap();
    assert_eq!(restored.connections.len(), 1);
    let connection = &restored.connections[0];
    assert_eq!(connection.port_type, pw_graph_core::PortType::Video);
    assert_eq!(connection.output_name, "video_out");
    assert_eq!(connection.input_name, "video_in");

    // Activation resolves the persisted names/types against the graph.
    let mut fresh = InMemoryDriver::demo();
    let fresh_first = fresh
        .create_video_filter(VideoFilterRequest {
            instance_id: "persist-a".into(),
            filter_id: "passthrough".into(),
            params: pw_graph_video::filters::FilterParams::default(),
            position: [0.0, 0.0],
        })
        .unwrap();
    let fresh_second = fresh
        .create_video_filter(VideoFilterRequest {
            instance_id: "persist-b".into(),
            filter_id: "grayscale".into(),
            params: pw_graph_video::filters::FilterParams::default(),
            position: [0.0, 0.0],
        })
        .unwrap();
    let report = restored.activate(&mut fresh, false, false).unwrap();
    assert_eq!(report.connected, 1, "{report:?}");
    assert!(fresh
        .graph()
        .links
        .values()
        .any(|link| link.output_port == fresh_first.output_port
            && link.input_port == fresh_second.input_port));
}
