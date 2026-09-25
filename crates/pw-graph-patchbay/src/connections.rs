//! Persistent connection sets and activation policies.
//!
//! The native qpwgraph format is XML and resolves rules by node/port names.
//! JSON remains supported as a convenient machine-readable format for tooling
//! and for compatibility with the first Rust prototype.

use super::model::{PatchConnection, Patchbay, PatchbayResolution, ReconcileReport};
use super::selectors::{
    connection_selectors, legacy_selector, resolve_selector, selectors_equivalent,
};
use pw_graph_backend::GraphDriver;
use pw_graph_core::{Direction, EndpointResolution, Graph, NodeType, PortId, PortKey};
use std::fmt::Write as FmtWrite;

impl Patchbay {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            version: 2,
            name: name.into(),
            connections: Vec::new(),
        }
    }

    pub fn add_connection(&mut self, output_port: PortId, input_port: PortId, pinned: bool) {
        if let Some(connection) = self.connections.iter_mut().find(|connection| {
            connection.output_port == output_port && connection.input_port == input_port
        }) {
            connection.pinned |= pinned;
            return;
        }
        self.connections.push(PatchConnection {
            output_port,
            input_port,
            pinned,
            ..PatchConnection::default()
        });
    }

    pub fn add_graph_connection(
        &mut self,
        graph: &Graph,
        output_port: PortId,
        input_port: PortId,
        pinned: bool,
    ) {
        let Some(output) = graph.port(output_port) else {
            self.add_connection(output_port, input_port, pinned);
            return;
        };
        let Some(input) = graph.port(input_port) else {
            self.add_connection(output_port, input_port, pinned);
            return;
        };
        let Some(output_node) = graph.node(output.node_id) else {
            self.add_connection(output_port, input_port, pinned);
            return;
        };
        let Some(input_node) = graph.node(input.node_id) else {
            self.add_connection(output_port, input_port, pinned);
            return;
        };
        let output_selector = graph.port_key(output_port).map(|key| key.selector());
        let input_selector = graph.port_key(input_port).map(|key| key.selector());
        if let Some(connection) = self.connections.iter_mut().find(|connection| {
            let selector_match = connection
                .output_selector
                .as_ref()
                .zip(connection.input_selector.as_ref())
                .zip(output_selector.as_ref().zip(input_selector.as_ref()))
                .is_some_and(
                    |((saved_output, saved_input), (current_output, current_input))| {
                        selectors_equivalent(saved_output, current_output)
                            && selectors_equivalent(saved_input, current_input)
                    },
                );
            let legacy_match = connection.output_selector.is_none()
                && connection.input_selector.is_none()
                && connection.output_node == output_node.name
                && connection.output_name == output.name
                && connection.input_node == input_node.name
                && connection.input_name == input.name;
            (connection.output_port == output_port && connection.input_port == input_port)
                || selector_match
                || legacy_match
        }) {
            connection.pinned |= pinned;
            connection.output_port = output_port;
            connection.input_port = input_port;
            connection.node_type = output_node.node_type;
            connection.output_node_type = Some(output_node.node_type);
            connection.input_node_type = Some(input_node.node_type);
            connection.port_type = output.port_type;
            connection.output_node = output_node.name.clone();
            connection.output_name = output.name.clone();
            connection.input_node = input_node.name.clone();
            connection.input_name = input.name.clone();
            connection.output_selector = output_selector;
            connection.input_selector = input_selector;
            return;
        }
        self.connections.push(PatchConnection {
            output_port,
            input_port,
            pinned,
            node_type: output_node.node_type,
            output_node_type: Some(output_node.node_type),
            input_node_type: Some(input_node.node_type),
            port_type: output.port_type,
            output_node: output_node.name.clone(),
            output_name: output.name.clone(),
            input_node: input_node.name.clone(),
            input_name: input.name.clone(),
            output_selector,
            input_selector,
        });
    }

    pub fn remove_connection(&mut self, output_port: PortId, input_port: PortId) -> bool {
        let original_len = self.connections.len();
        self.connections.retain(|connection| {
            connection.output_port != output_port || connection.input_port != input_port
        });
        original_len != self.connections.len()
    }

    /// Remove a saved rule by stable endpoint identity. Numeric PipeWire IDs
    /// can change while an application is paused, so deleting a newly
    /// recreated link must also remove the older saved rule.
    pub fn remove_stable_connection(&mut self, output: &PortKey, input: &PortKey) -> bool {
        let original_len = self.connections.len();
        self.connections
            .retain(|connection| !connection.matches_stable_pair(output, input));
        original_len != self.connections.len()
    }

    /// Remove every saved rule touching a node whose stable name is no longer
    /// present. This is used when an effect instance is deliberately removed;
    /// ordinary graph refreshes must retain unresolved rules for later
    /// activation.
    pub fn remove_connections_for_node(&mut self, node_name: &str) -> bool {
        let original_len = self.connections.len();
        self.connections.retain(|connection| {
            connection.output_node != node_name && connection.input_node != node_name
        });
        original_len != self.connections.len()
    }

    pub fn snapshot_graph(&mut self, graph: &Graph, pinned: bool) {
        let links: Vec<_> = graph.links.values().cloned().collect();
        self.connections.clear();
        for link in links {
            self.add_graph_connection(graph, link.output_port, link.input_port, pinned);
        }
    }

    /// Snapshot only links that the driver can mutate. Observed relationships
    /// may still be displayed in the live graph, but persisting them as
    /// reconnectable rules would make a later activation appear corrupted.
    pub fn snapshot_driver(&mut self, driver: &dyn GraphDriver, pinned: bool) {
        let graph = driver.graph();
        let links: Vec<_> = graph
            .links
            .values()
            .filter(|link| driver.is_link_mutable(link.id))
            .cloned()
            .collect();
        self.connections.clear();
        for link in links {
            self.add_graph_connection(graph, link.output_port, link.input_port, pinned);
        }
    }

    /// Return only rules that touch an effect endpoint. Effect routing is part
    /// of the persisted effect-node state and can be restored independently of
    /// the user's optional full patchbay-on-startup setting.
    pub fn effect_connections(&self) -> Self {
        Self {
            version: self.version,
            name: self.name.clone(),
            connections: self
                .connections
                .iter()
                .filter(|connection| {
                    connection.effective_output_node_type() == NodeType::Effect
                        || connection.effective_input_node_type() == NodeType::Effect
                })
                .cloned()
                .collect(),
        }
    }

    /// Resolve one persisted rule without mutating the graph. This is also
    /// used by the continuous reconciler so missing and ambiguous endpoints
    /// remain observable rather than being silently dropped.
    pub fn resolve_rule(&self, graph: &Graph, rule_index: usize) -> PatchbayResolution {
        let Some(connection) = self.connections.get(rule_index) else {
            return PatchbayResolution::WaitingForEndpoint {
                detail: format!("rule {rule_index} is no longer present"),
            };
        };
        let has_names = !connection.output_node.is_empty()
            && !connection.output_name.is_empty()
            && !connection.input_node.is_empty()
            && !connection.input_name.is_empty();
        if !has_names && connection.output_selector.is_none() && connection.input_selector.is_none()
        {
            let Some(output) = graph.port(connection.output_port) else {
                return PatchbayResolution::WaitingForEndpoint {
                    detail: "legacy output port is missing".into(),
                };
            };
            let Some(input) = graph.port(connection.input_port) else {
                return PatchbayResolution::WaitingForEndpoint {
                    detail: "legacy input port is missing".into(),
                };
            };
            return if output.direction == Direction::Source && input.direction == Direction::Sink {
                PatchbayResolution::Resolved {
                    output: output.id,
                    input: input.id,
                }
            } else {
                PatchbayResolution::WaitingForEndpoint {
                    detail: "legacy numeric endpoints have incompatible directions".into(),
                }
            };
        }
        let output = resolve_selector(graph, connection.output_selector.as_ref(), || {
            legacy_selector(
                connection.output_node.clone(),
                connection.output_name.clone(),
                Direction::Source,
                connection.port_type,
                connection.effective_output_node_type(),
            )
        });
        let input = resolve_selector(graph, connection.input_selector.as_ref(), || {
            legacy_selector(
                connection.input_node.clone(),
                connection.input_name.clone(),
                Direction::Sink,
                connection.port_type,
                connection.effective_input_node_type(),
            )
        });
        match (output, input) {
            (
                EndpointResolution::Exact(output) | EndpointResolution::UniqueFallback(output),
                EndpointResolution::Exact(input) | EndpointResolution::UniqueFallback(input),
            ) => PatchbayResolution::Resolved { output, input },
            (EndpointResolution::Ambiguous(ids), _) => PatchbayResolution::Ambiguous {
                detail: format!("output endpoint is ambiguous: {} candidates", ids.len()),
            },
            (_, EndpointResolution::Ambiguous(ids)) => PatchbayResolution::Ambiguous {
                detail: format!("input endpoint is ambiguous: {} candidates", ids.len()),
            },
            (EndpointResolution::Missing, _) => PatchbayResolution::WaitingForEndpoint {
                detail: "output endpoint is missing".into(),
            },
            (_, EndpointResolution::Missing) => PatchbayResolution::WaitingForEndpoint {
                detail: "input endpoint is missing".into(),
            },
        }
    }

    /// Build a detailed, on-demand report for the patchbay diagnostics view.
    ///
    /// This is intentionally not part of the regular model synchronization
    /// path: it formats every saved selector and current resolution, which is
    /// useful when investigating a recreated stream but unnecessary for the
    /// compact rule cards.
    pub fn debug_report(&self, graph: &Graph, report: &ReconcileReport) -> String {
        let mut text = String::new();
        let _ = writeln!(text, "Patchbay diagnostics");
        let _ = writeln!(text, "name={}", self.name);
        let _ = writeln!(text, "schema_version={}", self.version);
        let _ = writeln!(text, "saved_rules={}", self.connections.len());
        let _ = writeln!(text, "reconcile_connected={}", report.connected);
        let _ = writeln!(text, "reconcile_already_present={}", report.already_present);
        let _ = writeln!(text, "reconcile_disconnected={}", report.disconnected);
        if !report.warnings.is_empty() {
            let _ = writeln!(text, "warnings={:?}", report.warnings);
        }
        let _ = writeln!(text, "\nRules:");
        for (index, connection) in self.connections.iter().enumerate() {
            let (output, input) = connection_selectors(connection);
            let _ = writeln!(
                text,
                "rule[{index}] pinned={} display={}:{} -> {}:{}",
                connection.pinned,
                connection.output_node,
                connection.output_name,
                connection.input_node,
                connection.input_name,
            );
            let _ = writeln!(text, "  output_selector={output:?}");
            let _ = writeln!(
                text,
                "  output_resolution={}",
                graph.endpoint_resolution_explanation(&output)
            );
            let _ = writeln!(text, "  input_selector={input:?}");
            let _ = writeln!(
                text,
                "  input_resolution={}",
                graph.endpoint_resolution_explanation(&input)
            );
            if let Some(status) = report
                .rules
                .iter()
                .find(|status| status.rule_index == index)
            {
                let _ = writeln!(
                    text,
                    "  reconcile_status={} detail={}",
                    status.status.label(),
                    status.detail
                );
            } else {
                let _ = writeln!(text, "  reconcile_status=not sampled in the last pass");
            }
        }
        text
    }
}
