//! Core graph types shared by every backend and presentation layer.

use super::endpoint::{
    EndpointMatchMode, EndpointResolution, EndpointSelector, NodeType, PortType,
};
use super::id::{LinkId, NodeId, PortId};
use super::matching::{endpoint_identity_basis, endpoint_match_score};
use super::node::{Link, Node, Port, PortKey};
use super::relay::legacy_relay_port_name;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use thiserror::Error;

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct Graph {
    pub nodes: BTreeMap<NodeId, Node>,
    pub ports: BTreeMap<PortId, Port>,
    pub links: BTreeMap<LinkId, Link>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum GraphError {
    #[error("node {0} already exists")]
    DuplicateNode(NodeId),
    #[error("port {0} already exists")]
    DuplicatePort(PortId),
    #[error("link {0} already exists")]
    DuplicateLink(LinkId),
    #[error("node {0} does not exist")]
    MissingNode(NodeId),
    #[error("port {0} does not exist")]
    MissingPort(PortId),
    #[error("port {0} belongs to node {1}, not node {2}")]
    PortNodeMismatch(PortId, NodeId, NodeId),
    #[error("source port {0} must be a source")]
    NotSource(PortId),
    #[error("destination port {0} must be a sink")]
    NotSink(PortId),
    #[error("ports {0} and {1} are not compatible")]
    IncompatiblePorts(PortId, PortId),
    #[error("ports {0} and {1} are already linked")]
    DuplicateConnection(PortId, PortId),
    #[error("link {0} does not exist")]
    MissingLink(LinkId),
}

impl Graph {
    pub fn add_node(&mut self, node: Node) -> Result<(), GraphError> {
        if self.nodes.contains_key(&node.id) {
            return Err(GraphError::DuplicateNode(node.id));
        }
        self.nodes.insert(node.id, node);
        Ok(())
    }

    pub fn add_port(&mut self, port: Port) -> Result<(), GraphError> {
        let node = self
            .nodes
            .get_mut(&port.node_id)
            .ok_or(GraphError::MissingNode(port.node_id))?;
        if self.ports.contains_key(&port.id) {
            return Err(GraphError::DuplicatePort(port.id));
        }
        // A node may arrive already listing this port -- merging two graphs
        // clones whole nodes and then re-adds their ports. Listing the same id
        // twice draws the port twice, gives it two pins, and lets the second
        // (phantom) pin capture the link that belongs to the first.
        if !node.ports.contains(&port.id) {
            node.ports.push(port.id);
        }
        self.ports.insert(port.id, port);
        Ok(())
    }

    pub fn remove_link(&mut self, link_id: LinkId) -> Result<Link, GraphError> {
        self.links
            .remove(&link_id)
            .ok_or(GraphError::MissingLink(link_id))
    }

    pub fn link(&self, link_id: LinkId) -> Option<&Link> {
        self.links.get(&link_id)
    }

    pub fn port(&self, port_id: PortId) -> Option<&Port> {
        self.ports.get(&port_id)
    }

    pub fn port_key(&self, port_id: PortId) -> Option<PortKey> {
        let port = self.port(port_id)?;
        let node = self.node(port.node_id)?;
        Some(PortKey {
            node_name: node.name.clone(),
            node_serial: node.serial,
            node_type: node.node_type,
            port_name: port.name.clone(),
            channel: port.channel.clone(),
            direction: port.direction,
            port_type: port.port_type,
            identity: Some(node.matching_identity()),
            match_mode: EndpointMatchMode::Instance,
        })
    }

    /// Resolve a stable port key against the current registry snapshot.
    /// Serial is preferred, but a name fallback is intentional: a playback
    /// stream often receives a new serial when it is resumed.
    pub fn resolve_port_key(&self, key: &PortKey) -> Option<PortId> {
        // A patchbay saved before the relay ports were role-prefixed names
        // them `FL`/`FR`; accept that one rewrite so those files keep
        // reconnecting. See [`crate::legacy_relay_port_name`] for its scope.
        self.resolve_endpoint(&key.selector()).port_id()
    }

    /// Resolve a stable key while retaining an ambiguity explanation for
    /// patchbay and diagnostic callers.
    pub fn resolve_port_key_result(&self, key: &PortKey) -> EndpointResolution {
        self.resolve_endpoint(&key.selector())
    }

    /// Explain the current selector result for a support/debug report. The
    /// resolver itself stays typed and compact; this control-plane helper
    /// turns the selected identity tier into a human-readable reason without
    /// exposing a numeric PipeWire id as if it were durable identity.
    pub fn endpoint_resolution_explanation(&self, selector: &EndpointSelector) -> String {
        let basis = endpoint_identity_basis(selector);
        match self.resolve_endpoint(selector) {
            EndpointResolution::Exact(id) => format!("matched port {id} via {basis}"),
            EndpointResolution::UniqueFallback(id) => {
                format!("matched port {id} via fallback {basis}")
            }
            EndpointResolution::Ambiguous(ids) => {
                format!("ambiguous ({}) via {basis}: {ids:?}", ids.len())
            }
            EndpointResolution::Missing => format!("missing via {basis}"),
        }
    }

    /// Resolve a typed selector against the current registry snapshot.
    ///
    /// Scores express identity confidence, not a preference for a numeric
    /// object id. Equal best scores are returned as Ambiguous so a recreated
    /// application stream cannot be connected to the wrong same-named node.
    pub fn resolve_endpoint(&self, selector: &EndpointSelector) -> EndpointResolution {
        let mut candidates: Vec<(u16, PortId)> = self
            .ports
            .values()
            .filter(|port| {
                let exact_name = port.name == selector.port_name
                    || legacy_relay_port_name(
                        &selector.identity.node_name,
                        &selector.port_name,
                        selector.direction,
                    )
                    .is_some_and(|name| port.name == name);
                // A selector with application identity can describe a stream
                // channel rather than a particular PipeWire port spelling.
                // This lets a recreated browser/Electron stream resolve when
                // its port name changes, while the channel and direction
                // filters below still prevent a left/right swap. A
                // name-pattern selector remains port-name specific.
                let has_application_identity = selector.identity.application_id.is_some()
                    || selector.identity.process_binary.is_some()
                    || selector.identity.application_name.is_some();
                let application_channel =
                    !matches!(selector.match_mode, EndpointMatchMode::NamePattern)
                        && has_application_identity
                        && selector.channel.is_some()
                        && selector.channel.as_ref() == port.channel.as_ref();
                exact_name || application_channel
            })
            .filter(|port| port.direction == selector.direction)
            .filter(|port| {
                port.port_type == selector.port_type
                    || port.port_type == PortType::Unknown
                    || selector.port_type == PortType::Unknown
            })
            .filter_map(|port| {
                let node = self.node(port.node_id)?;
                if selector.node_type != NodeType::Unknown && node.node_type != selector.node_type {
                    return None;
                }
                let has_application_identity = selector.identity.application_id.is_some()
                    || selector.identity.process_binary.is_some()
                    || selector.identity.application_name.is_some();
                let application_channel =
                    !matches!(selector.match_mode, EndpointMatchMode::NamePattern)
                        && has_application_identity
                        && selector.channel.is_some()
                        && selector.channel.as_ref() == port.channel.as_ref();
                // The compatibility relay rewrite must remain tied to the
                // relay node named by the saved selector.
                if port.name != selector.port_name
                    && node.name != selector.identity.node_name
                    && node.matching_identity().node_name != selector.identity.node_name
                    && !application_channel
                {
                    return None;
                }
                if let (Some(expected), Some(actual)) =
                    (selector.channel.as_ref(), port.channel.as_ref())
                {
                    if expected != actual {
                        return None;
                    }
                }
                let score = endpoint_match_score(node, selector)?;
                Some((score, port.id))
            })
            .collect();

        if candidates.is_empty() {
            return EndpointResolution::Missing;
        }
        candidates.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
        let best_score = candidates[0].0;
        let best: Vec<_> = candidates
            .iter()
            .take_while(|(score, _)| *score == best_score)
            .map(|(_, id)| *id)
            .collect();
        if best.len() > 1 {
            return EndpointResolution::Ambiguous(best);
        }
        if best_score >= 800 {
            EndpointResolution::Exact(best[0])
        } else {
            EndpointResolution::UniqueFallback(best[0])
        }
    }

    pub fn find_link_by_keys(&self, output: &PortKey, input: &PortKey) -> Option<Link> {
        let output_id = self.resolve_port_key(output)?;
        let input_id = self.resolve_port_key(input)?;
        self.links
            .values()
            .find(|link| link.output_port == output_id && link.input_port == input_id)
            .cloned()
    }

    pub fn node(&self, node_id: NodeId) -> Option<&Node> {
        self.nodes.get(&node_id)
    }

    pub fn add_link(
        &mut self,
        link_id: LinkId,
        output_port: PortId,
        input_port: PortId,
    ) -> Result<Link, GraphError> {
        if self.links.contains_key(&link_id) {
            return Err(GraphError::DuplicateLink(link_id));
        }
        let output = self
            .ports
            .get(&output_port)
            .ok_or(GraphError::MissingPort(output_port))?;
        let input = self
            .ports
            .get(&input_port)
            .ok_or(GraphError::MissingPort(input_port))?;
        if !output.direction.is_source() {
            return Err(GraphError::NotSource(output_port));
        }
        if !input.direction.is_sink() {
            return Err(GraphError::NotSink(input_port));
        }
        if output.port_type != input.port_type {
            return Err(GraphError::IncompatiblePorts(output_port, input_port));
        }
        if self
            .links
            .values()
            .any(|link| link.output_port == output_port && link.input_port == input_port)
        {
            return Err(GraphError::DuplicateConnection(output_port, input_port));
        }
        let link = Link {
            id: link_id,
            output_port,
            input_port,
        };
        self.links.insert(link_id, link.clone());
        Ok(link)
    }

    /// Insert a link reported by a backend snapshot. Backends may know about
    /// legacy or partially-described links that cannot be revalidated locally.
    pub fn insert_existing_link(&mut self, link: Link) -> Result<(), GraphError> {
        if self.links.contains_key(&link.id) {
            return Err(GraphError::DuplicateLink(link.id));
        }
        if !self.ports.contains_key(&link.output_port) {
            return Err(GraphError::MissingPort(link.output_port));
        }
        if !self.ports.contains_key(&link.input_port) {
            return Err(GraphError::MissingPort(link.input_port));
        }
        self.links.insert(link.id, link);
        Ok(())
    }

    pub fn links_for_port(&self, port_id: PortId) -> impl Iterator<Item = &Link> {
        self.links
            .values()
            .filter(move |link| link.output_port == port_id || link.input_port == port_id)
    }
}
