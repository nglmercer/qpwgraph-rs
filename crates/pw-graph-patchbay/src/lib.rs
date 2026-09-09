//! Persistent connection sets and activation policies.
//!
//! The native qpwgraph format is XML and resolves rules by node/port names.
//! JSON remains supported as a convenient machine-readable format for tooling
//! and for compatibility with the first Rust prototype.

use pw_graph_backend::{BackendError, GraphDriver};
use pw_graph_core::{
    legacy_relay_port_name, Direction, Graph, NodeType, PortId, PortKey, PortType,
};
use serde::{Deserialize, Serialize};
use std::path::Path;
use thiserror::Error;

mod xml;

#[derive(Debug, Error)]
pub enum PatchbayError {
    #[error("could not read patchbay file: {0}")]
    Read(#[source] std::io::Error),
    #[error("could not write patchbay file: {0}")]
    Write(#[source] std::io::Error),
    #[error("invalid patchbay JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid patchbay XML: {0}")]
    Xml(#[from] quick_xml::Error),
    #[error("could not serialize patchbay XML: {0}")]
    XmlWrite(#[source] std::io::Error),
    #[error("patchbay XML contains invalid attributes")]
    XmlAttributes,
    #[error(transparent)]
    Backend(#[from] BackendError),
    /// Activation failed and one or both mutation directions could not be
    /// restored. The graph is in neither the previous nor the saved state, and
    /// the user has to be told rather than shown a bare error.
    #[error(
        "activation failed ({cause}); {created_links_left} created link(s) remain and {removed_links_not_restored} removed link(s) could not be restored"
    )]
    ActivationNotRolledBack {
        cause: String,
        created_links_left: usize,
        removed_links_not_restored: usize,
    },
}

/// Undo the complete mutation made by one activation. Created links are
/// removed first, in reverse order; links removed by exclusive or
/// auto-disconnect policy are then restored, also in reverse order. Both loops
/// deliberately continue after an error so the final report says which side
/// of the original graph remains stranded.
fn rollback_activation(
    driver: &mut dyn GraphDriver,
    created_by_activation: &[(PortKey, PortKey)],
    removed_by_activation: &[(PortKey, PortKey)],
    cause: BackendError,
) -> PatchbayError {
    let mut created_links_left = 0;
    for (output, input) in created_by_activation.iter().rev() {
        match driver.disconnect_by_key_if_present_without_suppression(output, input) {
            Ok(Some(_)) => driver.allow_connection(output, input),
            Ok(None) => {}
            Err(_) => created_links_left += 1,
        }
    }

    let mut removed_links_not_restored = 0;
    for (output, input) in removed_by_activation.iter().rev() {
        if driver.connect_by_key_if_missing(output, input).is_err() {
            removed_links_not_restored += 1;
        }
    }
    if created_links_left == 0 && removed_links_not_restored == 0 {
        PatchbayError::Backend(cause)
    } else {
        PatchbayError::ActivationNotRolledBack {
            cause: cause.to_string(),
            created_links_left,
            removed_links_not_restored,
        }
    }
}

/// Record a successful removal in the activation transaction. A route that
/// this same activation created and then removed is not part of the original
/// graph, so it must be forgotten from the created set rather than restored as
/// though it had existed before activation.
fn record_activation_removal(
    removed_by_activation: &mut Vec<(PortKey, PortKey)>,
    created_by_activation: &mut Vec<(PortKey, PortKey)>,
    output: PortKey,
    input: PortKey,
) {
    if let Some(index) = created_by_activation
        .iter()
        .position(|(created_output, created_input)| {
            created_output == &output && created_input == &input
        })
    {
        created_by_activation.remove(index);
    } else {
        removed_by_activation.push((output, input));
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct Patchbay {
    pub version: u32,
    pub name: String,
    pub connections: Vec<PatchConnection>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct PatchConnection {
    #[serde(default)]
    pub output_port: PortId,
    #[serde(default)]
    pub input_port: PortId,
    #[serde(default)]
    pub pinned: bool,
    /// Legacy output-side type used by the qpwgraph-compatible format.
    ///
    /// Older files have a single node type for both endpoints. Keep this
    /// field so those files remain readable, while the optional endpoint
    /// fields below retain the real type of each side for new files.
    #[serde(default)]
    pub node_type: NodeType,
    /// Explicit type of the output node. `None` denotes a legacy rule whose
    /// single `node_type` applied to both endpoints.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_node_type: Option<NodeType>,
    /// Explicit type of the input node. `None` denotes a legacy rule whose
    /// single `node_type` applied to both endpoints.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_node_type: Option<NodeType>,
    #[serde(default)]
    pub port_type: PortType,
    #[serde(default)]
    pub output_node: String,
    #[serde(default)]
    pub output_name: String,
    #[serde(default)]
    pub input_node: String,
    #[serde(default)]
    pub input_name: String,
}

impl PatchConnection {
    fn effective_output_node_type(&self) -> NodeType {
        self.output_node_type.unwrap_or(self.node_type)
    }

    fn effective_input_node_type(&self) -> NodeType {
        self.input_node_type.unwrap_or(self.node_type)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ActivationReport {
    pub connected: usize,
    pub already_present: usize,
    pub disconnected: usize,
    pub failed: Vec<String>,
}

impl Patchbay {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            version: 1,
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
        if let Some(connection) = self.connections.iter_mut().find(|connection| {
            (connection.output_port == output_port && connection.input_port == input_port)
                || (connection.output_node == output_node.name
                    && connection.output_name == output.name
                    && connection.input_node == input_node.name
                    && connection.input_name == input.name)
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
        self.connections.retain(|connection| {
            !(connection.output_node == output.node_name
                && connection.output_name == output.port_name
                && connection.input_node == input.node_name
                && connection.input_name == input.port_name)
        });
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

    pub fn save_to(&self, path: impl AsRef<Path>) -> Result<(), PatchbayError> {
        let path = path.as_ref();
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent).map_err(PatchbayError::Write)?;
        }
        let is_xml = path
            .extension()
            .and_then(|extension| extension.to_str())
            .map(|extension| matches!(extension.to_ascii_lowercase().as_str(), "qpwgraph" | "xml"))
            .unwrap_or(false);
        let text = if is_xml {
            self.to_xml()?
        } else {
            serde_json::to_string_pretty(self)?
        };
        pw_graph_utils::atomic_write(path, text.as_bytes(), false).map_err(PatchbayError::Write)
    }

    pub fn load_from(path: impl AsRef<Path>) -> Result<Self, PatchbayError> {
        let text = std::fs::read_to_string(path).map_err(PatchbayError::Read)?;
        if text.trim_start().starts_with('<') {
            Self::from_xml(&text)
        } else {
            Ok(serde_json::from_str(&text)?)
        }
    }

    /// Connect all saved edges. Name-based rules are resolved against the
    /// current registry snapshot, allowing IDs to change between sessions.
    /// Backend mutations are atomic: a fatal connect/disconnect error rolls
    /// back every mutation made by this invocation. Unresolved rules are
    /// skipped as before and are not mutations.
    pub fn activate(
        &self,
        driver: &mut dyn GraphDriver,
        exclusive: bool,
        auto_disconnect: bool,
    ) -> Result<ActivationReport, PatchbayError> {
        driver.refresh()?;
        let mut report = ActivationReport::default();
        let capabilities = driver.capabilities();
        if !self.connections.is_empty() && !capabilities.connect {
            report
                .failed
                .push("connection activation is not supported by this backend".into());
            return Ok(report);
        }
        if (exclusive || auto_disconnect) && !capabilities.disconnect {
            report
                .failed
                .push("connection removal is not supported by this backend".into());
            return Ok(report);
        }
        let resolved: Vec<(PortKey, PortKey)> = self
            .connections
            .iter()
            .filter_map(|connection| {
                let (output, input) = self.resolve_connection(driver.graph(), connection)?;
                Some((
                    driver.graph().port_key(output)?,
                    driver.graph().port_key(input)?,
                ))
            })
            .collect();

        let mut removed_by_activation: Vec<(PortKey, PortKey)> = Vec::new();
        let mut created_by_activation: Vec<(PortKey, PortKey)> = Vec::new();

        if exclusive {
            let live: Vec<_> = driver
                .graph()
                .links
                .values()
                // Observed relationships are not the patchbay's to remove.
                // A composite backend can expose immutable Core Audio session
                // links next to mutable MIDI ones and still report that it
                // supports disconnection overall.
                .filter(|link| driver.is_link_mutable(link.id))
                .filter_map(|link| {
                    Some((
                        driver.graph().port_key(link.output_port)?,
                        driver.graph().port_key(link.input_port)?,
                    ))
                })
                .collect();
            for (live_output, live_input) in live {
                let saved = resolved
                    .iter()
                    .any(|(output, input)| output == &live_output && input == &live_input);
                if saved {
                    continue;
                }
                match driver
                    .disconnect_by_key_if_present_without_suppression(&live_output, &live_input)
                {
                    Ok(Some(_)) => {
                        report.disconnected += 1;
                        record_activation_removal(
                            &mut removed_by_activation,
                            &mut created_by_activation,
                            live_output,
                            live_input,
                        );
                    }
                    Ok(None) => {}
                    Err(error) => {
                        return Err(rollback_activation(
                            driver,
                            &created_by_activation,
                            &removed_by_activation,
                            error,
                        ));
                    }
                }
            }
        }

        for (output, input) in resolved {
            if driver.graph().find_link_by_keys(&output, &input).is_some() {
                report.already_present += 1;
                continue;
            }

            if auto_disconnect {
                let Some(input_port) = driver.graph().resolve_port_key(&input) else {
                    continue;
                };
                let stale: Vec<(PortKey, PortKey)> = driver
                    .graph()
                    .links_for_port(input_port)
                    .filter(|link| link.input_port == input_port)
                    .filter(|link| driver.is_link_mutable(link.id))
                    .filter_map(|link| {
                        Some((
                            driver.graph().port_key(link.output_port)?,
                            driver.graph().port_key(link.input_port)?,
                        ))
                    })
                    .collect();
                for (stale_output, stale_input) in stale {
                    match driver.disconnect_by_key_if_present_without_suppression(
                        &stale_output,
                        &stale_input,
                    ) {
                        Ok(Some(_)) => {
                            report.disconnected += 1;
                            record_activation_removal(
                                &mut removed_by_activation,
                                &mut created_by_activation,
                                stale_output,
                                stale_input,
                            );
                        }
                        Ok(None) => {}
                        Err(error) => {
                            return Err(rollback_activation(
                                driver,
                                &created_by_activation,
                                &removed_by_activation,
                                error,
                            ));
                        }
                    }
                }
            }

            match driver.connect_by_key_if_missing(&output, &input) {
                Ok(Some(_)) => {
                    report.connected += 1;
                    created_by_activation.push((output, input));
                }
                Ok(None) => report.already_present += 1,
                Err(error) => {
                    return Err(rollback_activation(
                        driver,
                        &created_by_activation,
                        &removed_by_activation,
                        error,
                    ));
                }
            }
        }
        Ok(report)
    }

    fn resolve_connection(
        &self,
        graph: &Graph,
        connection: &PatchConnection,
    ) -> Option<(PortId, PortId)> {
        let has_names = !connection.output_node.is_empty()
            && !connection.output_name.is_empty()
            && !connection.input_node.is_empty()
            && !connection.input_name.is_empty();
        if has_names {
            let output = resolve_named_port(
                graph,
                &connection.output_node,
                &connection.output_name,
                Direction::Source,
                connection.port_type,
                connection.effective_output_node_type(),
                connection.output_node_type.is_none(),
            )?;
            let input = resolve_named_port(
                graph,
                &connection.input_node,
                &connection.input_name,
                Direction::Sink,
                connection.port_type,
                connection.effective_input_node_type(),
                connection.input_node_type.is_none(),
            )?;
            return Some((output, input));
        }

        // Legacy files may contain only numeric IDs. Keep that fallback, but
        // never let an old numeric ID override a complete name-based rule.
        let output = graph.port(connection.output_port)?;
        let input = graph.port(connection.input_port)?;
        (output.direction == Direction::Source && input.direction == Direction::Sink)
            .then_some((output.id, input.id))
    }
}

/// Resolve an endpoint by its durable node/port identity. Newer patchbay
/// rules carry an explicit endpoint type and must match it exactly. For a
/// legacy rule, first prefer the original shared type, then fall back to the
/// saved name/port shape when that type is no longer sufficient (for example
/// an old PipeWire-to-Effect rule saved before Effect had its own type).
fn resolve_named_port(
    graph: &Graph,
    node_name: &str,
    port_name: &str,
    direction: Direction,
    port_type: PortType,
    node_type: NodeType,
    allow_legacy_type_fallback: bool,
) -> Option<PortId> {
    let strict_type = (node_type != NodeType::Unknown).then_some(node_type);
    if let Some(port) = find_named_port(
        graph,
        node_name,
        port_name,
        direction,
        port_type,
        strict_type,
    ) {
        return Some(port);
    }

    allow_legacy_type_fallback
        .then(|| find_named_port(graph, node_name, port_name, direction, port_type, None))
        .flatten()
}

fn find_named_port(
    graph: &Graph,
    node_name: &str,
    port_name: &str,
    direction: Direction,
    port_type: PortType,
    node_type: Option<NodeType>,
) -> Option<PortId> {
    graph
        .nodes
        .values()
        .filter(|node| node.name == node_name)
        .filter(|node| node_type.is_none_or(|expected| node.node_type == expected))
        .find_map(|node| {
            // A patchbay saved before the relay filter ports were
            // role-prefixed stores them as bare `FL`/`FR`. `legacy_relay_port_name`
            // accepts that one rewrite, and only on the two relay nodes, so
            // an ordinary card's `FL` pin is untouched.
            let legacy_relay_port = legacy_relay_port_name(&node.name, port_name, direction);
            node.ports.iter().find_map(|id| {
                let port = graph.port(*id)?;
                ((port.name == port_name
                    || legacy_relay_port.is_some_and(|name| port.name == name))
                    && port.direction == direction
                    && (port_type == PortType::Unknown || port.port_type == port_type))
                    .then_some(port.id)
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pw_graph_backend::{
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

        fn disconnect(
            &mut self,
            link: pw_graph_core::LinkId,
        ) -> BackendResult<pw_graph_core::Link> {
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
        let path =
            std::env::temp_dir().join(format!("qpwgraph-rs-{}.qpwgraph", std::process::id()));
        patchbay.save_to(&path).unwrap();
        let loaded = Patchbay::load_from(&path).unwrap();
        assert_eq!(loaded.connections.len(), 1);
        assert_eq!(loaded.connections[0].output_node, "Audio Capture");
        assert_eq!(loaded.connections[0].output_name, "capture_FL");
        std::fs::remove_file(path).unwrap();
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
                channels: None,
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
                channels: None,
                position: [260.0, 180.0],
            })
            .unwrap();
        let report = saved.activate(&mut recreated, false, false).unwrap();

        assert_eq!(report.connected, 2);
        assert!(recreated
            .graph()
            .links
            .values()
            .any(|link| link.output_port == PortId(1)
                && link.input_port == recreated_effect.input_port));
        assert!(recreated
            .graph()
            .links
            .values()
            .any(|link| link.output_port == recreated_effect.output_port
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
        assert!(xml.contains("output-node-type=\"effect\""));
        assert!(xml.contains("input-node-type=\"pipewire\""));

        let loaded = Patchbay::from_xml(&xml).unwrap();
        let connection = &loaded.connections[0];
        assert_eq!(connection.effective_output_node_type(), NodeType::Effect);
        assert_eq!(connection.effective_input_node_type(), NodeType::PipeWire);

        let same_type = graph_with_named_audio_edge(NodeType::Effect, NodeType::Effect, 30, 40);
        let mut same_type_patchbay = Patchbay::new("effects");
        same_type_patchbay.add_graph_connection(&same_type, PortId(41), PortId(42), true);
        let same_type_xml = same_type_patchbay.to_xml().unwrap();
        assert!(!same_type_xml.contains("input-node-type"));
        let same_type_loaded = Patchbay::from_xml(&same_type_xml).unwrap();
        assert_eq!(
            same_type_loaded.connections[0].output_node_type,
            Some(NodeType::Effect)
        );
        assert_eq!(
            same_type_loaded.connections[0].input_node_type,
            Some(NodeType::Effect)
        );
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
}
