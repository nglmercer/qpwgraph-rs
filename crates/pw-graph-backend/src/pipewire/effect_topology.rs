//! Stable, control-plane topology helpers for PipeWire effect ports.
//!
//! The realtime callback can observe missing DSP buffers during a graph
//! renegotiation. These helpers intentionally use the rebuilt graph instead,
//! so a transient pointer does not become a persistent Hush generation reset.

use pw_graph_core::{Graph, NodeId, Port, PortType};

/// Map a PipeWire effect port's channel label to the compact DSP channel
/// index used by the effect callback.
pub(super) fn channel_index(port: &Port) -> Option<usize> {
    let label = port
        .channel
        .as_deref()
        .or_else(|| port.name.rsplit('_').next())?;
    match label.to_ascii_lowercase().as_str() {
        "mono" | "fl" | "fc" | "center" => Some(0),
        "fr" => Some(1),
        _ => None,
    }
}

/// Calculate which physical effect input channels have stable graph links.
/// This is intentionally pure, which makes the topology contract testable
/// without a running PipeWire daemon.
pub(super) fn active_channel_mask(graph: &Graph, node_id: NodeId, physical_channels: usize) -> u16 {
    let Some(node) = graph.node(node_id) else {
        return 0;
    };
    let mut mask = 0_u16;
    for port_id in &node.ports {
        let Some(port) = graph.port(*port_id) else {
            continue;
        };
        if !port.direction.is_sink() || port.port_type != PortType::Audio {
            continue;
        }
        let Some(channel) = channel_index(port) else {
            continue;
        };
        if channel >= physical_channels {
            continue;
        }
        if graph.links.values().any(|link| link.input_port == *port_id) {
            mask |= 1_u16 << channel;
        }
    }
    mask
}

#[cfg(test)]
mod tests {
    use super::*;
    use pw_graph_core::{Direction, LinkId, Node, NodeType, PortId};

    fn fixture() -> (Graph, NodeId, PortId, PortId, PortId) {
        let mut graph = Graph::default();
        let effect = NodeId(1);
        graph
            .add_node(Node::new(effect, "Hush", NodeType::Effect))
            .unwrap();
        let left = PortId(2);
        let right = PortId(3);
        graph
            .add_port(
                Port::new(left, effect, "input_FL", Direction::Sink, PortType::Audio)
                    .with_channel("FL"),
            )
            .unwrap();
        graph
            .add_port(
                Port::new(right, effect, "input_FR", Direction::Sink, PortType::Audio)
                    .with_channel("FR"),
            )
            .unwrap();
        let source = NodeId(4);
        graph
            .add_node(Node::new(source, "Microphone", NodeType::PipeWire))
            .unwrap();
        let source_port = PortId(5);
        graph
            .add_port(Port::new(
                source_port,
                source,
                "capture_FL",
                Direction::Source,
                PortType::Audio,
            ))
            .unwrap();
        (graph, effect, left, right, source_port)
    }

    #[test]
    fn active_mask_follows_stable_links_not_physical_capacity() {
        let (mut graph, effect, left, right, source_port) = fixture();
        assert_eq!(active_channel_mask(&graph, effect, 2), 0);

        graph.add_link(LinkId(10), source_port, left).unwrap();
        assert_eq!(active_channel_mask(&graph, effect, 2), 0b01);

        let right_source = PortId(6);
        graph
            .add_port(Port::new(
                right_source,
                NodeId(4),
                "capture_FR",
                Direction::Source,
                PortType::Audio,
            ))
            .unwrap();
        graph.add_link(LinkId(11), right_source, right).unwrap();
        assert_eq!(active_channel_mask(&graph, effect, 2), 0b11);

        graph.remove_link(LinkId(10)).unwrap();
        assert_eq!(active_channel_mask(&graph, effect, 2), 0b10);
    }

    #[test]
    fn mask_never_includes_channels_outside_physical_capacity() {
        let (mut graph, effect, left, right, source_port) = fixture();
        graph.add_link(LinkId(10), source_port, left).unwrap();
        let right_source = PortId(6);
        graph
            .add_port(Port::new(
                right_source,
                NodeId(4),
                "capture_FR",
                Direction::Source,
                PortType::Audio,
            ))
            .unwrap();
        graph.add_link(LinkId(11), right_source, right).unwrap();
        assert_eq!(active_channel_mask(&graph, effect, 1), 0b01);
    }
}
