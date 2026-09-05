use super::*;
use pw_graph_core::{backend_for_node, backend_for_port, BackendKind};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

#[test]
fn stable_ids_are_deterministic_and_namespaced() {
    let endpoint = graph_id(endpoint_node_local_id("speaker-id"));
    let port = graph_id(endpoint_port_local_id("speaker-id"));
    assert_eq!(endpoint, graph_id(endpoint_node_local_id("speaker-id")));
    assert_ne!(endpoint, port);
    assert_eq!(
        backend_for_node(NodeId(endpoint)),
        Some(BackendKind::WindowsAudio)
    );
    assert_eq!(
        backend_for_port(PortId(port)),
        Some(BackendKind::WindowsAudio)
    );
}

#[test]
fn session_link_identity_depends_on_both_native_identifiers() {
    assert_ne!(
        session_link_local_id("endpoint-a", "session-a"),
        session_link_local_id("endpoint-b", "session-a")
    );
    assert_ne!(
        session_link_local_id("endpoint-a", "session-a"),
        session_link_local_id("endpoint-a", "session-b")
    );
}

#[test]
fn a_playback_endpoints_monitor_has_an_identity_of_its_own() {
    let input = endpoint_port_local_id("speaker-id");
    let monitor = endpoint_monitor_port_local_id("speaker-id");
    // Two ports on one node: what the speakers play, and what they are
    // playing. Sharing an id would collapse them into one pin.
    assert_ne!(input, monitor);
    assert_eq!(monitor, endpoint_monitor_port_local_id("speaker-id"));
    assert_ne!(monitor, endpoint_monitor_port_local_id("other-id"));
}

#[test]
fn a_route_keeps_its_identity_across_a_rebuild_and_reverses_with_direction() {
    let (output, input) = (PortId(11), PortId(22));
    let link = managed_link(output, input);
    assert_eq!(link.output_port, output);
    assert_eq!(link.input_port, input);
    // Derived from the pair, so a graph rebuild finds the same link rather
    // than drawing a second one beside it.
    assert_eq!(managed_link(output, input).id, link.id);
    // Direction is part of the identity: A into B is not B into A.
    assert_ne!(managed_link(input, output).id, link.id);
    assert_eq!(
        backend_for_port(PortId(link.id.0)),
        Some(BackendKind::WindowsAudio)
    );
}

#[test]
fn endpoint_and_session_direction_mapping_matches_core_audio_flow() {
    assert_eq!(endpoint_direction(Audio::eRender), Direction::Sink);
    assert_eq!(endpoint_direction(Audio::eCapture), Direction::Source);
    assert_eq!(session_direction(Audio::eRender), Direction::Source);
    assert_eq!(session_direction(Audio::eCapture), Direction::Sink);

    let session_port = PortId(10);
    let endpoint_port = PortId(20);
    assert_eq!(
        session_link_ports(Audio::eRender, session_port, endpoint_port),
        (session_port, endpoint_port)
    );
    assert_eq!(
        session_link_ports(Audio::eCapture, session_port, endpoint_port),
        (endpoint_port, session_port)
    );
}

#[test]
fn endpoint_notifications_mark_the_graph_dirty() {
    let dirty = Arc::new(AtomicBool::new(false));
    let topology_dirty = Arc::new(AtomicBool::new(false));
    let callback: Audio::IMMNotificationClient = EndpointNotificationClient {
        dirty: Arc::clone(&dirty),
        topology_dirty: Arc::clone(&topology_dirty),
        #[cfg(feature = "relay")]
        default_generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
    }
    .into();

    unsafe {
        callback
            .OnDeviceAdded(PCWSTR(std::ptr::null()))
            .expect("notification callback should accept a device event");
    }
    assert!(dirty.load(Ordering::Acquire));
    assert!(topology_dirty.load(Ordering::Acquire));
}

#[test]
fn session_notifications_record_only_the_owning_endpoint() {
    let dirty = Arc::new(AtomicBool::new(false));
    let endpoints = Arc::new(Mutex::new(BTreeSet::new()));
    mark_session_endpoint_dirty(&dirty, &endpoints, "endpoint-a");
    mark_session_endpoint_dirty(&dirty, &endpoints, "endpoint-a");
    assert!(dirty.load(Ordering::Acquire));
    assert_eq!(
        take_session_dirty_endpoints(&endpoints),
        BTreeSet::from(["endpoint-a".into()])
    );
    assert!(take_session_dirty_endpoints(&endpoints).is_empty());
}

#[test]
fn a_valid_volume_callback_promotes_an_initially_unknown_state() {
    let states: AudioStateMap = Arc::new(Mutex::new(BTreeMap::new()));
    let node = NodeId(42);

    apply_state_change(&states, node, 0.4, true);

    let state = states.lock().unwrap()[&node];
    assert_eq!(state.volume, Some(0.4));
    assert_eq!(state.muted, Some(true));
    assert!(state.volume_readable);
    assert!(state.mute_readable);
}

#[test]
fn live_backend_startup_is_optional_for_headless_windows_ci() {
    let Ok(mut driver) = WindowsAudioDriver::new() else {
        // Windows CI runners may not expose an audio service or endpoint.
        return;
    };
    let nodes = driver
        .refresh()
        .expect("Core Audio refresh should succeed after startup");
    assert!(nodes.iter().all(|node| {
        matches!(
            node.node_type,
            NodeType::WindowsAudioEndpoint | NodeType::WindowsAudioSession
        )
    }));
    assert!(driver
        .graph()
        .ports
        .values()
        .all(|port| port.port_type == PortType::Audio));
    assert!(driver.graph().links.values().all(|link| {
        driver.graph().port(link.output_port).is_some()
            && driver.graph().port(link.input_port).is_some()
    }));
}

#[test]
fn every_playback_endpoint_offers_a_monitor_alongside_its_input() {
    let Ok(driver) = WindowsAudioDriver::new() else {
        return;
    };
    let graph = driver.graph();
    for node in graph.nodes.values() {
        if node.node_type != NodeType::WindowsAudioEndpoint {
            continue;
        }
        let directions: Vec<Direction> = node
            .ports
            .iter()
            .filter_map(|port| graph.port(*port))
            .map(|port| port.direction)
            .collect();
        // A capture endpoint is a source and nothing else. A playback
        // endpoint is a sink plus the monitor that makes "send what these
        // speakers are playing somewhere else" drawable.
        match directions.len() {
            1 => assert_eq!(directions[0], Direction::Source),
            2 => {
                assert!(directions.contains(&Direction::Sink));
                assert!(directions.contains(&Direction::Source));
            }
            other => panic!("an endpoint node had {other} ports"),
        }
    }
}

#[test]
fn only_endpoints_offer_a_connect_gesture() {
    let Ok(driver) = WindowsAudioDriver::new() else {
        return;
    };
    for node in driver.graph().nodes.values() {
        let routable = driver.node_supports_routing(node.id);
        match node.node_type {
            // An application session is drawn, selectable, and metered, but
            // Windows exposes no supported way to move one.  The one
            // documented exception is an app that the user already moved to
            // QPWGraph Virtual Output; its process-loopback port is a real
            // qpwgraph-owned source.
            NodeType::WindowsAudioSession => {
                let has_process_port = node.ports.iter().any(|port| {
                    matches!(
                        driver
                            .endpoint_ports
                            .get(port)
                            .map(|endpoint| endpoint.role),
                        Some(EndpointPortRole::Process { .. })
                    )
                });
                assert_eq!(routable, has_process_port, "{} routing mismatch", node.name);
            }
            _ => assert!(routable, "{} was not routable", node.name),
        }
    }
}

#[test]
fn a_session_port_is_refused_with_an_explanation_rather_than_drawn() {
    let Ok(mut driver) = WindowsAudioDriver::new() else {
        return;
    };
    let graph = driver.graph();
    let session_port = graph.ports.values().find(|port| {
        graph
            .node(port.node_id)
            .is_some_and(|node| node.node_type == NodeType::WindowsAudioSession)
    });
    let render_port = graph.ports.values().find(|port| {
        port.direction.is_sink()
            && graph
                .node(port.node_id)
                .is_some_and(|node| node.node_type == NodeType::WindowsAudioEndpoint)
    });
    let (Some(session_port), Some(render_port)) = (session_port, render_port) else {
        // No application was playing when the test ran.
        return;
    };
    let (session_port, render_port) = (session_port.id, render_port.id);

    let error = driver
        .connect(session_port, render_port)
        .expect_err("an application session cannot be re-pointed");

    // Unsupported, not Native: this is a thing Windows does not offer, not a
    // call that went wrong.
    assert!(
        matches!(error, BackendError::Unsupported(_) | BackendError::Graph(_)),
        "expected an explained refusal, got {error:?}"
    );
    assert!(driver
        .graph()
        .link(managed_link(session_port, render_port).id)
        .is_none());
}

#[test]
fn an_observed_session_link_is_never_mutable() {
    let Ok(driver) = WindowsAudioDriver::new() else {
        return;
    };
    // Nothing is routed yet, so every link in the graph came from Core Audio
    // and none of them may reach patchbay persistence or a reroute command.
    for link in driver.graph().links.values() {
        assert!(!driver.is_link_mutable(link.id));
    }
}

#[test]
fn an_endpoint_can_be_routed_to_a_playback_endpoint_when_enabled() {
    // Opt-in: this one opens real WASAPI streams and moves real audio, so it
    // needs a machine with working devices.
    if std::env::var_os("PW_GRAPH_TEST_LINKS").is_none() {
        return;
    }
    let mut driver = WindowsAudioDriver::new().expect("Core Audio should be available");
    let graph = driver.graph();
    // Never the same device on both ends. A playback endpoint's monitor
    // routed back into itself is a digital feedback loop, and each pass
    // through it is louder than the last.
    let pair = graph
        .ports
        .values()
        .filter(|port| port.direction.is_source())
        .find_map(|source| {
            graph
                .ports
                .values()
                .find(|render| render.direction.is_sink() && render.node_id != source.node_id)
                .map(|render| (source.id, render.id))
        });
    let Some((source, render)) = pair else {
        // A machine with no playback device, or with nothing to feed it.
        return;
    };

    let link = driver
        .connect(source, render)
        .expect("an endpoint into a playback endpoint is a route Windows can carry");
    assert!(driver.graph().link(link.id).is_some());
    // A route qpwgraph carries is mutable; the observed ones next to it are
    // still not.
    assert!(driver.is_link_mutable(link.id));

    // The route survives a rebuild that knows nothing about it.
    driver.refresh().expect("refresh should succeed");
    assert!(driver.graph().link(link.id).is_some());

    driver
        .disconnect(link.id)
        .expect("the route should stop cleanly");
    assert!(driver.graph().link(link.id).is_none());
    assert!(!driver.is_link_mutable(link.id));
}
