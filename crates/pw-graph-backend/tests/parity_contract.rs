//! Contract every backend has to satisfy, regardless of platform.
//!
//! These tests exist because Linux and Windows drivers are compiled on
//! different machines and cannot both be exercised in one run. Anything that
//! can be expressed as platform-neutral data is tested here, so a rule that
//! both backends depend on is verified wherever the suite runs. Behaviour that
//! genuinely needs a live daemon stays in the driver's own opt-in tests.

use pw_graph_backend::{
    is_measurable_audio_node, nodes_to_meter, spa_volume_to_ui_volume, ui_volume_to_spa_volume,
    BackendError, DemoDriver, EffectDriver, EffectInsertRequest, GraphDriver, MeterPolicy,
    NodeAudioState, NodeCapabilities,
};
use pw_graph_core::{NodeId, NodeType, PortId};
use std::collections::{BTreeMap, BTreeSet};

// === Meters ===============================================================

/// A playback device is metered through its monitor, so having only input
/// ports must not disqualify it.
#[test]
fn playback_sink_is_eligible_for_a_meter() {
    assert!(is_measurable_audio_node("Audio/Sink", false, true));
}

/// A capture device is metered directly from its source port.
#[test]
fn capture_source_is_eligible_for_a_meter() {
    assert!(is_measurable_audio_node("Audio/Source", true, false));
}

#[test]
fn a_node_with_no_audio_in_either_direction_is_never_eligible() {
    assert!(!is_measurable_audio_node("Audio/Sink", false, false));
    assert!(!is_measurable_audio_node("Video/Source", false, false));
}

#[test]
fn disabled_policy_meters_nothing_even_when_asked() {
    let measurable = BTreeSet::from([NodeId(1), NodeId(2)]);
    let requested = BTreeSet::from([NodeId(1)]);
    assert!(nodes_to_meter(MeterPolicy::Disabled, &measurable, &requested).is_empty());
}

#[test]
fn on_demand_policy_meters_only_what_was_requested() {
    let measurable = BTreeSet::from([NodeId(1), NodeId(2)]);
    let requested = BTreeSet::from([NodeId(2)]);
    assert_eq!(
        nodes_to_meter(MeterPolicy::OnDemand, &measurable, &requested),
        BTreeSet::from([NodeId(2)])
    );
    // Nothing requested means nothing metered: a plain launch attaches no
    // helper streams to the user's audio graph.
    assert!(nodes_to_meter(MeterPolicy::OnDemand, &measurable, &BTreeSet::new()).is_empty());
}

/// Requesting a meter for something that cannot be measured must not open a
/// stream for it.
#[test]
fn on_demand_policy_ignores_requests_for_unmeasurable_nodes() {
    let measurable = BTreeSet::from([NodeId(1)]);
    let requested = BTreeSet::from([NodeId(99)]);
    assert!(nodes_to_meter(MeterPolicy::OnDemand, &measurable, &requested).is_empty());
}

#[test]
fn always_policy_meters_every_eligible_node() {
    let measurable = BTreeSet::from([NodeId(1), NodeId(2)]);
    assert_eq!(
        nodes_to_meter(MeterPolicy::Always, &measurable, &BTreeSet::new()),
        measurable
    );
}

/// A backend that cannot report its own changes must not claim it can: the
/// application drops to a slow reconciliation timer for backends that do, so a
/// false claim here would freeze the graph instead of merely wasting work.
#[test]
fn a_backend_without_change_notifications_asks_to_be_polled() {
    let driver = DemoDriver::demo();
    assert!(!driver.reports_graph_changes());
    assert!(!driver.graph_dirty());
}

/// PipeWire stores gain as amplitude cubed. Getting the pair wrong makes a
/// fader feel correct only at unity and silence, which is easy to miss by eye
/// and impossible to miss here.
#[test]
fn spa_gain_round_trips_through_the_cubed_curve() {
    for linear in [0.0, 0.25, 0.5, 0.75, 1.0, 1.5] {
        let stored = ui_volume_to_spa_volume(linear, 1.5);
        let back = spa_volume_to_ui_volume(stored);
        assert!(
            (back - linear).abs() < 1e-4,
            "{linear} stored as {stored} came back as {back}"
        );
    }
    // Half gain is stored as an eighth, which is the whole point of the curve.
    assert!((ui_volume_to_spa_volume(0.5, 1.5) - 0.125).abs() < 1e-6);
    // Out-of-range input is clamped rather than producing NaN.
    assert!(ui_volume_to_spa_volume(-1.0, 1.5).is_finite());
    assert!(spa_volume_to_ui_volume(-1.0).is_finite());
    assert_eq!(ui_volume_to_spa_volume(9.0, 1.0), 1.0);
}

// === Audio state ==========================================================

/// The first thing the UI does is read; there must be a real value waiting,
/// not a placeholder the UI invented.
#[test]
fn initial_state_is_read_from_the_backend() {
    let driver = DemoDriver::demo();
    let node = *driver.graph().nodes.keys().next().expect("demo has nodes");

    let state = driver.node_audio_state(node).expect("state is readable");

    assert!(state.volume_readable && state.mute_readable);
    assert_eq!(state.volume, Some(1.0), "demo nodes start at unity");
    assert_eq!(state.muted, Some(false));
}

#[test]
fn a_write_is_visible_in_the_next_read() {
    let mut driver = DemoDriver::demo();
    let node = *driver.graph().nodes.keys().next().expect("demo has nodes");

    driver
        .set_node_volume(node, 0.25)
        .expect("volume is writable");
    driver.set_node_mute(node, true).expect("mute is writable");

    let state = driver.node_audio_state(node).expect("state is readable");
    assert_eq!(state.volume, Some(0.25));
    assert_eq!(state.muted, Some(true));
}

#[test]
fn an_unknown_node_reports_a_missing_node_error() {
    let driver = DemoDriver::demo();
    let missing = NodeId(u64::MAX);

    let error = driver.node_audio_state(missing).unwrap_err();

    assert!(
        matches!(error, BackendError::Graph(_)),
        "a node that does not exist is an error, not an empty reading: {error}"
    );
}

/// A backend that cannot read a node says so instead of returning a number.
/// `None` and `false` are the honest answer; a fabricated level is not.
#[test]
fn an_unsupported_node_reports_no_value_rather_than_a_default() {
    let state = NodeAudioState::UNSUPPORTED;

    assert_eq!(state.volume, None);
    assert_eq!(state.muted, None);
    assert!(!state.is_supported());
}

// === Capabilities =========================================================

#[test]
fn a_controllable_node_reports_every_supported_operation() {
    let driver = DemoDriver::demo();
    let node = *driver.graph().nodes.keys().next().expect("demo has nodes");

    let capabilities = driver.node_capabilities(node);

    assert_eq!(capabilities, NodeCapabilities::FULL);
    assert!(capabilities.has_any_control());
    assert!(capabilities.has_any_meter());
}

/// The UI draws controls from this, so a node with nothing to control must
/// report nothing to control -- otherwise a dead fader is rendered.
#[test]
fn an_effect_node_offers_no_audio_controls_or_meter() {
    let mut driver = DemoDriver::demo();
    driver
        .connect(PortId(1), PortId(3))
        .expect("demo pair links");
    let source = driver.graph().port_key(PortId(1)).expect("source key");
    let destination = driver.graph().port_key(PortId(3)).expect("destination key");
    let instance = driver
        .insert_effect(EffectInsertRequest {
            instance_id: "parity-effect".into(),
            effect_id: pw_graph_effects::NOISE_GATE_ID.into(),
            module_path: None,
            source,
            destination,
            enabled: true,
            parameters: BTreeMap::new(),
            channels: None,
            position: [0.0, 0.0],
        })
        .expect("demo backend hosts effects");
    assert_eq!(
        driver.graph().nodes[&instance.node_id].node_type,
        NodeType::Effect
    );

    let state = driver
        .node_audio_state(instance.node_id)
        .expect("an effect node still answers");
    assert!(!state.is_supported(), "DSP nodes carry no audio controls");

    let capabilities = driver.node_capabilities(instance.node_id);

    assert!(!capabilities.has_any_control());
    assert!(!capabilities.has_any_meter());
}

#[test]
fn an_unknown_node_supports_nothing() {
    let driver = DemoDriver::demo();

    assert_eq!(
        driver.node_capabilities(NodeId(u64::MAX)),
        NodeCapabilities::NONE
    );
}

/// The gate the UI uses: a control is only drawn when the node can be written.
#[test]
fn a_read_only_node_offers_no_writable_control() {
    let read_only = NodeAudioState {
        volume: Some(0.5),
        muted: Some(false),
        volume_readable: true,
        volume_writable: false,
        mute_readable: true,
        mute_writable: false,
    };

    let capabilities = read_only.control_capabilities();

    assert!(capabilities.volume_read && capabilities.mute_read);
    assert!(!capabilities.volume_write && !capabilities.mute_write);
    // It still counts as having controls, because the values are worth showing.
    assert!(capabilities.has_any_control());
}
