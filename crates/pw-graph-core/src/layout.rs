//! Core graph types shared by every backend and presentation layer.

use super::endpoint::{Direction, NodeType, PortType};
use super::graph::Graph;
use super::id::NodeId;
use super::node::Node;
use std::collections::BTreeMap;

impl Graph {
    /// Suggest a readable, deterministic layout for every node in the graph.
    ///
    /// Connected nodes are assigned to layers following the direction of
    /// their links, so sources appear before their sinks and multi-hop graphs
    /// spread across several columns. Nodes in a layer are then ordered by the
    /// barycentre of their neighbours, which keeps parallel paths aligned and
    /// reduces edge crossings. Hardware capture, application, processing, and
    /// output nodes are kept in stable visual lanes, while disconnected nodes
    /// are placed after connected nodes with an explicit gap. This means a
    /// newly discovered microphone cannot be inserted into the middle of an
    /// existing application route just because its name sorts there.
    pub fn default_node_positions(&self) -> BTreeMap<NodeId, [f32; 2]> {
        const LAYOUT_LEFT: f32 = 40.0;
        const LAYOUT_TOP: f32 = 40.0;
        const LAYOUT_X_STEP: f32 = 360.0;
        const LAYOUT_ROW_GAP: f32 = 70.0;
        const LAYOUT_LANE_GAP: f32 = 28.0;
        const LAYOUT_DISCONNECTED_GAP: f32 = 64.0;

        if self.nodes.is_empty() {
            return BTreeMap::new();
        }

        let mut incoming_count: BTreeMap<NodeId, usize> =
            self.nodes.keys().copied().map(|node| (node, 0)).collect();
        let mut incoming: BTreeMap<NodeId, Vec<NodeId>> = self
            .nodes
            .keys()
            .copied()
            .map(|node| (node, Vec::new()))
            .collect();
        let mut outgoing: BTreeMap<NodeId, Vec<NodeId>> = self
            .nodes
            .keys()
            .copied()
            .map(|node| (node, Vec::new()))
            .collect();
        for link in self.links.values() {
            let (Some(output), Some(input)) =
                (self.port(link.output_port), self.port(link.input_port))
            else {
                continue;
            };
            if output.node_id == input.node_id
                || !self.nodes.contains_key(&output.node_id)
                || !self.nodes.contains_key(&input.node_id)
            {
                continue;
            }
            outgoing
                .entry(output.node_id)
                .or_default()
                .push(input.node_id);
            incoming
                .entry(input.node_id)
                .or_default()
                .push(output.node_id);
            if let Some(count) = incoming_count.get_mut(&input.node_id) {
                *count += 1;
            }
        }
        for targets in outgoing.values_mut() {
            targets.sort_unstable();
            targets.dedup();
        }
        for sources in incoming.values_mut() {
            sources.sort_unstable();
            sources.dedup();
        }

        let node_limit = self.nodes.len().saturating_sub(1);
        let mut graph_layers: BTreeMap<NodeId, usize> =
            self.nodes.keys().copied().map(|node| (node, 0)).collect();
        let mut queue: std::collections::VecDeque<NodeId> = incoming_count
            .iter()
            .filter_map(|(node, count)| (*count == 0).then_some(*node))
            .collect();
        if queue.is_empty() {
            queue.extend(self.nodes.keys().copied().take(1));
        }
        while let Some(node_id) = queue.pop_front() {
            let current_layer = graph_layers.get(&node_id).copied().unwrap_or_default();
            for target in outgoing.get(&node_id).into_iter().flatten() {
                let candidate = (current_layer + 1).min(node_limit);
                let target_layer = graph_layers.entry(*target).or_default();
                if candidate > *target_layer {
                    *target_layer = candidate;
                    queue.push_back(*target);
                }
            }
        }

        // A disconnected cycle has no zero-indegree root. Seed each remaining
        // component deterministically so it still receives a useful layer.
        for node_id in self.nodes.keys().copied() {
            if incoming_count.get(&node_id).copied().unwrap_or_default() > 0
                && graph_layers.get(&node_id).copied().unwrap_or_default() == 0
            {
                queue.push_back(node_id);
                while let Some(current) = queue.pop_front() {
                    let current_layer = graph_layers.get(&current).copied().unwrap_or_default();
                    for target in outgoing.get(&current).into_iter().flatten() {
                        let candidate = (current_layer + 1).min(node_limit);
                        let target_layer = graph_layers.entry(*target).or_default();
                        if candidate > *target_layer {
                            *target_layer = candidate;
                            queue.push_back(*target);
                        }
                    }
                }
            }
        }

        let max_layer = graph_layers
            .values()
            .copied()
            .max()
            .unwrap_or_default()
            .max(2);
        let mut layers = vec![Vec::new(); max_layer + 1];
        let mut layer_by_node = BTreeMap::new();
        for node in self.nodes.values() {
            let graph_layer = graph_layers.get(&node.id).copied().unwrap_or_default();
            let role_layer = match self.node_layout_role(node) {
                0 => 0,
                1 => 1,
                2 => 2,
                _ => 0,
            };
            let layer = if graph_layer == 0 {
                role_layer
            } else {
                graph_layer
            };
            if layer >= layers.len() {
                layers.resize_with(layer + 1, Vec::new);
            }
            layers[layer].push(node.id);
            layer_by_node.insert(node.id, layer);
        }

        for nodes in &mut layers {
            nodes.sort_by(|left, right| {
                let left_node = self.nodes.get(left).expect("layout node exists");
                let right_node = self.nodes.get(right).expect("layout node exists");
                self.node_layout_lane(left_node)
                    .cmp(&self.node_layout_lane(right_node))
                    .then_with(|| {
                        self.node_media_category(left_node)
                            .cmp(&self.node_media_category(right_node))
                    })
                    .then_with(|| {
                        self.node_layout_role(left_node)
                            .cmp(&self.node_layout_role(right_node))
                    })
                    .then_with(|| {
                        left_node
                            .name
                            .to_ascii_lowercase()
                            .cmp(&right_node.name.to_ascii_lowercase())
                    })
                    .then_with(|| left.cmp(right))
            });
        }

        self.order_layout_layers(&mut layers, &incoming, &outgoing, &layer_by_node);

        let mut positions = BTreeMap::new();
        for (layer, nodes) in layers.into_iter().enumerate() {
            let mut top = LAYOUT_TOP;
            let mut previous_connected = None;
            let mut previous_lane = None;
            for node_id in nodes {
                let node = self.nodes.get(&node_id).expect("layout node exists");
                let connected = self.node_is_connected(node_id, &incoming, &outgoing);
                let lane = self.node_layout_lane(node);
                if previous_connected == Some(true) && !connected {
                    top += LAYOUT_DISCONNECTED_GAP;
                } else if previous_connected == Some(connected)
                    && previous_lane.is_some()
                    && previous_lane != Some(lane)
                {
                    top += LAYOUT_LANE_GAP;
                }
                positions.insert(node_id, [LAYOUT_LEFT + layer as f32 * LAYOUT_X_STEP, top]);
                top += self.node_layout_height(node) + LAYOUT_ROW_GAP;
                previous_connected = Some(connected);
                previous_lane = Some(lane);
            }
        }
        positions
    }

    fn node_media_category(&self, node: &Node) -> u8 {
        let mut has_audio = false;
        let mut has_video = false;
        let mut has_midi = false;
        for port_id in &node.ports {
            match self.port(*port_id).map(|port| port.port_type) {
                Some(PortType::Audio) => has_audio = true,
                Some(PortType::Video) => has_video = true,
                Some(PortType::MidiJack | PortType::MidiAlsa) => has_midi = true,
                _ => {}
            }
        }
        if has_audio {
            0
        } else if has_video {
            1
        } else if has_midi {
            2
        } else {
            3
        }
    }

    fn node_layout_role(&self, node: &Node) -> u8 {
        let mut has_source = false;
        let mut has_sink = false;
        for port_id in &node.ports {
            match self.port(*port_id).map(|port| port.direction) {
                Some(Direction::Source) => has_source = true,
                Some(Direction::Sink) => has_sink = true,
                None => {}
            }
        }
        match (has_source, has_sink) {
            (true, false) => 0,
            (false, true) => 2,
            _ => 1,
        }
    }

    /// Return the visual lane used for stable grouping inside one graph layer:
    /// hardware capture, application, processing/duplex, and output.
    fn node_layout_lane(&self, node: &Node) -> u8 {
        if node.node_type == NodeType::Recorder {
            return 4;
        }
        if node.node_type == NodeType::Effect {
            return 2;
        }

        let identity = node.matching_identity();
        if node.node_type == NodeType::WindowsAudioSession || identity.has_application_identity() {
            return 1;
        }

        let mut has_source = false;
        let mut has_sink = false;
        for port_id in &node.ports {
            match self.port(*port_id).map(|port| port.direction) {
                Some(Direction::Source) => has_source = true,
                Some(Direction::Sink) => has_sink = true,
                None => {}
            }
        }
        if has_sink {
            3
        } else if has_source {
            0
        } else {
            2
        }
    }

    fn node_layout_height(&self, node: &Node) -> f32 {
        let has_audio = node.ports.iter().any(|port_id| {
            self.port(*port_id)
                .is_some_and(|port| port.port_type == PortType::Audio)
        });
        let panel_height = if node.node_type == NodeType::Recorder {
            68.0
        } else if has_audio {
            42.0
        } else {
            0.0
        };
        (42.0 + panel_height + 14.0 + node.ports.len() as f32 * 25.0).max(62.0)
    }

    fn node_is_connected(
        &self,
        node_id: NodeId,
        incoming: &BTreeMap<NodeId, Vec<NodeId>>,
        outgoing: &BTreeMap<NodeId, Vec<NodeId>>,
    ) -> bool {
        incoming
            .get(&node_id)
            .is_some_and(|nodes| !nodes.is_empty())
            || outgoing
                .get(&node_id)
                .is_some_and(|nodes| !nodes.is_empty())
    }

    fn order_layout_layers(
        &self,
        layers: &mut [Vec<NodeId>],
        incoming: &BTreeMap<NodeId, Vec<NodeId>>,
        outgoing: &BTreeMap<NodeId, Vec<NodeId>>,
        layer_by_node: &BTreeMap<NodeId, usize>,
    ) {
        // A few alternating barycentre sweeps are enough for the small graph
        // shown by the canvas. The static lane/name tie-breakers keep every
        // result deterministic when a graph is symmetric or disconnected.
        for _ in 0..4 {
            for forward in [true, false] {
                let layer_indices: Vec<_> = if forward {
                    (0..layers.len()).collect()
                } else {
                    (0..layers.len()).rev().collect()
                };
                for layer in layer_indices {
                    let order = Self::layout_order_indices(layers);
                    self.sort_layout_layer(
                        &mut layers[layer],
                        layer,
                        forward,
                        incoming,
                        outgoing,
                        &order,
                        layer_by_node,
                    );
                }
            }
        }
    }

    fn layout_order_indices(layers: &[Vec<NodeId>]) -> BTreeMap<NodeId, usize> {
        let mut order = BTreeMap::new();
        for nodes in layers {
            for (index, node_id) in nodes.iter().copied().enumerate() {
                order.insert(node_id, index);
            }
        }
        order
    }

    #[allow(clippy::too_many_arguments)]
    fn sort_layout_layer(
        &self,
        nodes: &mut [NodeId],
        layer: usize,
        forward: bool,
        incoming: &BTreeMap<NodeId, Vec<NodeId>>,
        outgoing: &BTreeMap<NodeId, Vec<NodeId>>,
        order: &BTreeMap<NodeId, usize>,
        layer_by_node: &BTreeMap<NodeId, usize>,
    ) {
        nodes.sort_by(|left, right| {
            let left_connected = self.node_is_connected(*left, incoming, outgoing);
            let right_connected = self.node_is_connected(*right, incoming, outgoing);
            let connection_order = (!left_connected).cmp(&(!right_connected));
            if connection_order != std::cmp::Ordering::Equal {
                return connection_order;
            }
            // The initial static sort establishes the deterministic order for
            // ties. Keeping it when a sweep has no neighbours prevents a
            // later reverse sweep from undoing an edge-aware order with an
            // unrelated alphabetical comparison.
            if !left_connected {
                return std::cmp::Ordering::Equal;
            }
            let left_center = self.layout_neighbor_center(
                *left,
                layer,
                forward,
                incoming,
                outgoing,
                order,
                layer_by_node,
            );
            let right_center = self.layout_neighbor_center(
                *right,
                layer,
                forward,
                incoming,
                outgoing,
                order,
                layer_by_node,
            );
            match (left_center, right_center) {
                (Some(left), Some(right)) => left.total_cmp(&right),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            }
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn layout_neighbor_center(
        &self,
        node_id: NodeId,
        layer: usize,
        forward: bool,
        incoming: &BTreeMap<NodeId, Vec<NodeId>>,
        outgoing: &BTreeMap<NodeId, Vec<NodeId>>,
        order: &BTreeMap<NodeId, usize>,
        layer_by_node: &BTreeMap<NodeId, usize>,
    ) -> Option<f64> {
        let neighbours = if forward {
            incoming.get(&node_id)
        } else {
            outgoing.get(&node_id)
        }?;
        let mut total = 0.0;
        let mut count = 0_u32;
        for neighbour in neighbours {
            let neighbour_layer = layer_by_node.get(neighbour).copied().unwrap_or(layer);
            let points_in_sweep_direction = if forward {
                neighbour_layer < layer
            } else {
                neighbour_layer > layer
            };
            if points_in_sweep_direction {
                if let Some(index) = order.get(neighbour) {
                    total += *index as f64;
                    count += 1;
                }
            }
        }
        (count != 0).then_some(total / f64::from(count))
    }
}
