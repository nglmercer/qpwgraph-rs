//! Stable `i32` identities for Slint.
//!
//! Slint models are keyed by plain integers, so every graph object needs an
//! identity that survives a rebuild. The map is the only thing that mints
//! them, and it is rebuilt from the graph rather than persisted.

use super::*;

/// Maps opaque backend IDs to nonzero Slint `int` values. It never casts the
/// original u64 values, so high-bit ALSA IDs and future PipeWire IDs remain
/// safe in the UI.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct SlintIdMap {
    pub(super) next: i32,
    pub(super) nodes: BTreeMap<NodeId, i32>,
    pub(super) ports: BTreeMap<PortId, i32>,
    pub(super) links: BTreeMap<LinkId, i32>,
    reverse_nodes: HashMap<i32, NodeId>,
    reverse_ports: HashMap<i32, PortId>,
    reverse_links: HashMap<i32, LinkId>,
}

impl SlintIdMap {
    pub(crate) fn rebuild(&mut self, graph: &Graph) {
        self.nodes.retain(|id, _| graph.nodes.contains_key(id));
        self.ports.retain(|id, _| graph.ports.contains_key(id));
        self.links.retain(|id, _| graph.links.contains_key(id));
        for id in graph.nodes.keys() {
            self.allocate_node(*id);
        }
        for id in graph.ports.keys() {
            self.allocate_port(*id);
        }
        for id in graph.links.keys() {
            self.allocate_link(*id);
        }
        self.rebuild_reverse();
    }

    fn rebuild_reverse(&mut self) {
        self.reverse_nodes = self
            .nodes
            .iter()
            .map(|(id, mapped)| (*mapped, *id))
            .collect();
        self.reverse_ports = self
            .ports
            .iter()
            .map(|(id, mapped)| (*mapped, *id))
            .collect();
        self.reverse_links = self
            .links
            .iter()
            .map(|(id, mapped)| (*mapped, *id))
            .collect();
    }

    pub(crate) fn node(&self, id: NodeId) -> Option<i32> {
        self.nodes.get(&id).copied()
    }

    pub(crate) fn port(&self, id: PortId) -> Option<i32> {
        self.ports.get(&id).copied()
    }

    pub(crate) fn port_id(&self, id: i32) -> Option<PortId> {
        self.reverse_ports.get(&id).copied()
    }

    pub(crate) fn link(&self, id: LinkId) -> Option<i32> {
        self.links.get(&id).copied()
    }

    pub(crate) fn node_id(&self, id: i32) -> Option<NodeId> {
        self.reverse_nodes.get(&id).copied()
    }

    pub(crate) fn link_id(&self, id: i32) -> Option<LinkId> {
        self.reverse_links.get(&id).copied()
    }

    pub(super) fn allocate_node(&mut self, id: NodeId) {
        if !self.nodes.contains_key(&id) {
            let next = self.next_id();
            self.nodes.insert(id, next);
            self.reverse_nodes.insert(next, id);
        }
    }

    pub(super) fn allocate_port(&mut self, id: PortId) {
        if !self.ports.contains_key(&id) {
            let next = self.next_id();
            self.ports.insert(id, next);
            self.reverse_ports.insert(next, id);
        }
    }

    pub(super) fn allocate_link(&mut self, id: LinkId) {
        if !self.links.contains_key(&id) {
            let next = self.next_id();
            self.links.insert(id, next);
            self.reverse_links.insert(next, id);
        }
    }

    pub(super) fn next_id(&mut self) -> i32 {
        self.next = self.next.max(1);
        let id = self.next;
        self.next = self.next.checked_add(1).unwrap_or(1);
        id
    }
}
