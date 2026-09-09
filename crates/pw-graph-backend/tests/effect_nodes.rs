//! Integration coverage for the public effect-node API.
//!
//! These tests deliberately exercise the deterministic backend through its
//! public traits so the canvas can rely on the same behavior as other
//! backends: an effect may be created as an ordinary patchable node, or
//! inserted into (and later removed from) an existing route.

use pw_graph_backend::{
    DemoDriver, EffectDriver, EffectInsertRequest, EffectNodeRequest, GraphDriver,
};
use pw_graph_core::{Direction, NodeType, PortId, PortType};
use pw_graph_effects::{ChannelPolicy, NOISE_GATE_ID, NOISE_GATE_THRESHOLD};
use std::collections::BTreeMap;

fn effect_request(instance_id: &str) -> EffectNodeRequest {
    EffectNodeRequest {
        instance_id: instance_id.into(),
        effect_id: NOISE_GATE_ID.into(),
        module_path: None,
        enabled: true,
        parameters: BTreeMap::new(),
        channel_policy: ChannelPolicy::Auto,
        position: [240.0, 160.0],
    }
}

#[test]
fn standalone_effect_node_can_be_manually_patched_and_removed() {
    let mut driver = DemoDriver::demo();

    let instance = driver
        .create_effect_node(effect_request("standalone-gate"))
        .expect("a built-in effect should create a graph node");

    assert!(driver.supports_effect_nodes());
    assert_eq!(instance.source, None);
    assert_eq!(instance.destination, None);
    assert_eq!(instance.config.instance_id, "standalone-gate");
    assert_eq!(instance.config.effect_id, NOISE_GATE_ID);
    assert_eq!(
        driver.graph().nodes[&instance.node_id].node_type,
        NodeType::Effect
    );
    assert_eq!(
        driver.graph().nodes[&instance.node_id]
            .effect_instance_id
            .as_deref(),
        Some("standalone-gate")
    );
    assert_eq!(
        driver.graph().nodes[&instance.node_id].position,
        [240.0, 160.0]
    );
    assert_eq!(
        driver.graph().port(instance.input_port).unwrap().direction,
        Direction::Sink
    );
    assert_eq!(
        driver.graph().port(instance.output_port).unwrap().direction,
        Direction::Source
    );
    assert_eq!(
        driver.graph().port(instance.input_port).unwrap().port_type,
        PortType::Audio
    );
    assert_eq!(
        driver.graph().port(instance.output_port).unwrap().port_type,
        PortType::Audio
    );

    // The standalone API produces ordinary audio ports, so a canvas can make
    // both connections through the normal graph driver.
    driver
        .connect(PortId(1), instance.input_port)
        .expect("capture should connect to the effect input");
    driver
        .connect(instance.output_port, PortId(3))
        .expect("effect output should connect to playback");
    assert_eq!(driver.graph().links.len(), 2);

    driver
        .remove_effect("standalone-gate")
        .expect("removing a standalone effect should remove only its links");
    assert!(driver.effect_instances().is_empty());
    assert_eq!(driver.graph().nodes.len(), 4);
    assert_eq!(driver.graph().ports.len(), 6);
    assert!(driver.graph().links.is_empty());
}

#[test]
fn matching_effect_instances_keep_distinct_stable_port_keys() {
    let mut driver = DemoDriver::demo();
    let first = driver
        .create_effect_node(effect_request("gate-a"))
        .expect("the first gate should be created");
    let second = driver
        .create_effect_node(effect_request("gate-b"))
        .expect("the second gate should be created");

    let first_input = driver.graph().port_key(first.input_port).unwrap();
    let second_input = driver.graph().port_key(second.input_port).unwrap();
    let first_output = driver.graph().port_key(first.output_port).unwrap();
    let second_output = driver.graph().port_key(second.output_port).unwrap();

    assert_ne!(first_input.node_name, second_input.node_name);
    assert_ne!(first_output.node_name, second_output.node_name);
    assert_eq!(
        driver.graph().resolve_port_key(&first_input),
        Some(first.input_port)
    );
    assert_eq!(
        driver.graph().resolve_port_key(&second_input),
        Some(second.input_port)
    );
    assert_eq!(
        driver.graph().resolve_port_key(&first_output),
        Some(first.output_port)
    );
    assert_eq!(
        driver.graph().resolve_port_key(&second_output),
        Some(second.output_port)
    );
}

#[test]
fn inserted_effect_restores_the_original_route_when_removed() {
    let mut driver = DemoDriver::demo();
    driver
        .connect(PortId(1), PortId(3))
        .expect("the direct route should exist before insertion");
    let source = driver.graph().port_key(PortId(1)).unwrap();
    let destination = driver.graph().port_key(PortId(3)).unwrap();

    let instance = driver
        .insert_effect(EffectInsertRequest {
            instance_id: "inserted-gate".into(),
            effect_id: NOISE_GATE_ID.into(),
            module_path: None,
            source: source.clone(),
            destination: destination.clone(),
            enabled: true,
            parameters: BTreeMap::new(),
            channel_policy: ChannelPolicy::Auto,
            position: [310.0, 190.0],
        })
        .expect("insertion should replace the direct route with an effect");

    assert_eq!(instance.source.as_ref(), Some(&source));
    assert_eq!(instance.destination.as_ref(), Some(&destination));
    assert_eq!(
        driver.graph().nodes[&instance.node_id].position,
        [310.0, 190.0]
    );
    assert!(driver
        .graph()
        .find_link_by_keys(&source, &destination)
        .is_none());
    assert!(driver
        .graph()
        .links
        .values()
        .any(|link| { link.output_port == PortId(1) && link.input_port == instance.input_port }));
    assert!(driver
        .graph()
        .links
        .values()
        .any(|link| { link.output_port == instance.output_port && link.input_port == PortId(3) }));

    driver
        .remove_effect("inserted-gate")
        .expect("removing an inserted effect should restore its route");
    assert!(driver.effect_instances().is_empty());
    assert_eq!(driver.graph().nodes.len(), 4);
    assert_eq!(driver.graph().ports.len(), 6);
    assert_eq!(driver.graph().links.len(), 1);
    assert!(driver
        .graph()
        .find_link_by_keys(&source, &destination)
        .is_some());
}

#[test]
fn effect_node_creation_rejects_invalid_and_duplicate_requests_atomically() {
    let mut driver = DemoDriver::demo();
    let initial_graph = driver.graph().clone();

    let mut unknown_parameter = effect_request("bad-parameter");
    unknown_parameter
        .parameters
        .insert("not-a-noise-gate-parameter".into(), 1.0);
    assert!(driver.create_effect_node(unknown_parameter).is_err());
    assert_eq!(driver.graph(), &initial_graph);
    assert!(driver.effect_instances().is_empty());

    let mut unknown_effect = effect_request("unknown-effect");
    unknown_effect.effect_id = "does-not-exist".into();
    assert!(driver.create_effect_node(unknown_effect).is_err());
    assert_eq!(driver.graph(), &initial_graph);
    assert!(driver.effect_instances().is_empty());

    let created = driver
        .create_effect_node(effect_request("kept-effect"))
        .expect("the valid request should create an effect");
    let graph_after_create = driver.graph().clone();
    let instances_after_create = driver.effect_instances();

    let mut duplicate = effect_request("kept-effect");
    duplicate.position = [999.0, 999.0];
    assert!(driver.create_effect_node(duplicate).is_err());
    assert_eq!(driver.graph(), &graph_after_create);
    assert_eq!(driver.effect_instances(), instances_after_create);
    assert_eq!(driver.effect_instances()[0].node_id, created.node_id);
}

#[test]
fn effect_configuration_is_preserved_and_parameter_updates_are_validated() {
    let mut driver = DemoDriver::demo();
    let mut request = effect_request("configured-gate");
    request.module_path = Some("effects/noise-gate.wasm".into());
    request.enabled = false;
    request
        .parameters
        .insert(NOISE_GATE_THRESHOLD.into(), -24.0);

    let instance = driver
        .create_effect_node(request)
        .expect("valid configuration should be applied before the node is exposed");
    assert_eq!(
        instance.config.module_path.as_deref(),
        Some("effects/noise-gate.wasm")
    );
    assert!(!instance.config.enabled);
    assert_eq!(
        instance.config.parameters.get(NOISE_GATE_THRESHOLD),
        Some(&-24.0)
    );

    driver
        .set_effect_enabled("configured-gate", true)
        .expect("the effect should be addressable by its stable instance id");
    driver
        .set_effect_parameter("configured-gate", NOISE_GATE_THRESHOLD, -18.0)
        .expect("a known parameter should update the running processor and config");
    let configured = driver.effect_instances().pop().unwrap();
    assert!(configured.config.enabled);
    assert_eq!(
        configured.config.parameters.get(NOISE_GATE_THRESHOLD),
        Some(&-18.0)
    );

    let config_before_invalid_update = configured.config.clone();
    assert!(driver
        .set_effect_parameter("configured-gate", "not-a-noise-gate-parameter", 1.0)
        .is_err());
    assert_eq!(
        driver.effect_instances()[0].config,
        config_before_invalid_update,
        "an invalid runtime parameter must not be recorded as configuration"
    );
}
