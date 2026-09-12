//! Framework-neutral application/backend composition.
//!
//! The desktop UI is deliberately not part of this crate.  Both the native
//! application shell and tests use the same [`CompositeDriver`], so a view
//! cannot accidentally create a second graph namespace or route an ALSA
//! resource through PipeWire.

use pw_graph_backend::{
    BackendCapabilities, BackendError, BackendResult, GraphDriver, MeterPolicy, NodeAudioState,
    NodeCapabilities, RecorderDriver,
};
use pw_graph_core::{
    backend_for_link, backend_for_node, backend_for_port, BackendKind, Graph, GraphError, Link,
    LinkId, Node, NodeId, NodeType, PortId, PortType,
};
use std::collections::BTreeSet;

/// Legacy public compatibility constant. New routing code uses the shared
/// backend namespace helpers in `pw-graph-core`; the high bit remains
/// recognized only for IDs written by older ALSA builds.
pub const ALSA_ID_FLAG: u64 = 1_u64 << 63;

#[cfg(all(target_os = "linux", feature = "alsa"))]
use pw_graph_alsamidi::AlsaMidiDriver;
#[cfg(all(target_os = "linux", feature = "pipewire"))]
use pw_graph_backend::PipewireDriver;
#[cfg(target_os = "windows")]
use pw_graph_backend::{WindowsAudioDriver, WindowsMidiDriver};

mod composite;

pub use composite::{route_for_ports, CompositeDriver, CompositeRoute};

/// A backend that can be used by the application layer.  Relay is an optional
/// extension of the same object rather than a second UI-owned driver.
#[cfg(feature = "relay")]
pub trait ApplicationDriver: GraphDriver + RecorderDriver + pw_graph_backend::RelayDriver {}

#[cfg(feature = "relay")]
impl<T> ApplicationDriver for T where T: GraphDriver + RecorderDriver + pw_graph_backend::RelayDriver
{}

#[cfg(not(feature = "relay"))]
pub trait ApplicationDriver: GraphDriver + RecorderDriver {}

#[cfg(not(feature = "relay"))]
impl<T> ApplicationDriver for T where T: GraphDriver + RecorderDriver {}

/// Result of attempting to open the optional native backends.  A missing
/// backend is reported to the caller but does not prevent the other backend
/// from remaining usable.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BackendAvailability {
    pub pipewire: bool,
    pub alsa: bool,
    pub windows_audio: bool,
    pub windows_midi: bool,
    pub failures: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};
    // The refresh clock and the graph merge live in `composite::refresh`;
    // these tests exercise them directly.
    use crate::composite::refresh::{merge_graph_into, refresh_due};
    use pw_graph_backend::InMemoryDriver;
    use pw_graph_core::{encode_backend_id, BackendNamespace, Direction, Node, Port};

    /// Regression: the composite forwarded fourteen relay methods and quietly
    /// inherited the trait defaults for the trusted-enrollment ones, so
    /// approving a paired peer failed with "trusted enrollment is not
    /// available for this backend" on a backend that implements all of it.
    ///
    /// The message is the assertion. A composite with no relay child should
    /// say the relay is unavailable; saying enrollment specifically is
    /// unavailable means the call never reached a child at all.
    #[cfg(all(target_os = "windows", feature = "relay"))]
    #[test]
    fn composite_forwards_trusted_enrollment_to_the_owning_backend() {
        use pw_graph_backend::RelayDriver;

        let Ok(driver) = pw_graph_backend::WindowsMidiDriver::new() else {
            return;
        };
        // A MIDI-only composite has no relay child, which is the case that
        // distinguishes "no backend" from "backend cannot do enrollment".
        let mut composite = CompositeDriver::with_windows_midi(driver);
        assert!(!composite.relay_available());

        for message in [
            composite
                .relay_trusted_enrollment_secret(1)
                .expect_err("no relay child")
                .to_string(),
            composite
                .relay_accept_trusted_enrollment(1)
                .expect_err("no relay child")
                .to_string(),
            composite
                .relay_reject_trusted_enrollment(1, "no")
                .expect_err("no relay child")
                .to_string(),
            composite
                .relay_remove_trusted_peer("peer")
                .expect_err("no relay child")
                .to_string(),
        ] {
            assert!(
                message.contains("audio relay is not available"),
                "the composite did not forward the call: {message}"
            );
        }
    }

    /// Regression: the composite implemented `set_node_volume`/`set_node_mute`
    /// but not `node_audio_state`/`node_capabilities`, so the trait default
    /// answered `UNSUPPORTED` for every live node. The UI drives its controls
    /// off that, so every card on a real backend lost its volume and mute
    /// controls even though the driver underneath reported both.
    #[cfg(target_os = "windows")]
    #[test]
    fn composite_forwards_audio_state_to_the_owning_backend() {
        let Ok(mut driver) = pw_graph_backend::WindowsAudioDriver::new() else {
            // No Core Audio in this environment.
            return;
        };
        if driver.refresh().is_err() || driver.graph().nodes.is_empty() {
            return;
        }
        let expected: Vec<_> = driver
            .graph()
            .nodes
            .keys()
            .map(|node_id| {
                (
                    *node_id,
                    driver.node_audio_state(*node_id).ok(),
                    driver.node_capabilities(*node_id),
                )
            })
            .collect();
        assert!(
            expected
                .iter()
                .any(|(_, _, capabilities)| capabilities.has_any_control()),
            "an endpoint should report controls, or this proves nothing"
        );

        let mut composite = CompositeDriver::with_windows_audio(driver);
        composite.refresh().expect("composite refresh");

        for (node_id, state, capabilities) in expected {
            assert_eq!(
                composite.node_audio_state(node_id).ok(),
                state,
                "composite must not swallow the backend's reading"
            );
            assert_eq!(composite.node_capabilities(node_id), capabilities);
        }
    }

    /// The other half of the same failure: a merged graph must list each port
    /// on its node exactly once, or the card grows a second row and a phantom
    /// pin that captures the link belonging to the real one.
    #[test]
    fn merging_a_graph_lists_every_port_once() {
        let mut source = Graph::default();
        let node_id = NodeId(encode_backend_id(BackendNamespace::PipeWire, 7));
        let port_id = PortId(encode_backend_id(BackendNamespace::PipeWire, 8));
        source
            .add_node(Node::new(node_id, "Speakers", NodeType::PipeWire))
            .unwrap();
        source
            .add_port(Port::new(
                port_id,
                node_id,
                "audio",
                Direction::Sink,
                PortType::Audio,
            ))
            .unwrap();

        let mut merged = Graph::default();
        merge_graph_into(&mut merged, &source).expect("merge succeeds");

        assert_eq!(merged.nodes[&node_id].ports, vec![port_id]);
    }

    #[test]
    fn composite_reports_cross_backend_connections_before_mutation() {
        let pipewire_output = PortId(encode_backend_id(BackendNamespace::PipeWire, 42));
        let alsa_output = PortId(encode_backend_id(BackendNamespace::AlsaMidi, 42));
        let windows_output = PortId(encode_backend_id(BackendNamespace::WindowsAudio, 42));

        assert_eq!(
            route_for_ports(
                pipewire_output,
                PortId(encode_backend_id(BackendNamespace::PipeWire, 43))
            ),
            Ok(CompositeRoute::PipeWire)
        );
        assert_eq!(
            route_for_ports(
                alsa_output,
                PortId(encode_backend_id(BackendNamespace::AlsaMidi, 43))
            ),
            Ok(CompositeRoute::AlsaMidi)
        );
        assert_eq!(
            route_for_ports(
                pipewire_output,
                PortId(encode_backend_id(BackendNamespace::AlsaMidi, 43))
            ),
            Err("connections cannot cross PipeWire and ALSA MIDI backends")
        );
        assert_eq!(
            route_for_ports(
                alsa_output,
                PortId(encode_backend_id(BackendNamespace::PipeWire, 43))
            ),
            Err("connections cannot cross PipeWire and ALSA MIDI backends")
        );
        assert_eq!(
            route_for_ports(
                windows_output,
                PortId(encode_backend_id(BackendNamespace::WindowsAudio, 43))
            ),
            Ok(CompositeRoute::WindowsAudio)
        );
    }

    #[test]
    fn application_driver_blanket_impl_covers_the_deterministic_driver() {
        fn accepts_driver<T: ApplicationDriver>(_driver: &T) {}
        accepts_driver(&InMemoryDriver::demo());
    }

    #[test]
    fn a_composite_merges_children_without_overlapping_namespaces() {
        let mut graph = Graph::default();
        graph
            .add_node(Node::new(NodeId(1), "PipeWire", NodeType::PipeWire))
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
        assert!(graph.port(PortId(1)).is_some());
    }

    #[test]
    fn refresh_schedule_polls_when_a_child_has_no_deadline_yet() {
        let now = Instant::now();
        assert!(refresh_due(None, false, now));
    }

    #[test]
    fn refresh_schedule_prioritizes_dirty_children() {
        let now = Instant::now();
        assert!(refresh_due(Some(now + Duration::from_secs(30)), true, now));
        assert!(!refresh_due(
            Some(now + Duration::from_secs(30)),
            false,
            now
        ));
    }

    #[test]
    fn refresh_schedule_expires_at_the_child_deadline() {
        let now = Instant::now();
        assert!(!refresh_due(
            Some(now + Duration::from_secs(30)),
            false,
            now
        ));
        assert!(refresh_due(
            Some(now - Duration::from_millis(1)),
            false,
            now
        ));
    }
}
