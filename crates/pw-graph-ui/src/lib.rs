//! Framework-neutral graph presentation state.
//!
//! The application owns the backend and command stack. This crate owns the
//! projection from the backend graph into stable view data and the semantic
//! actions emitted by the Slint front end. Keeping this boundary free of a UI
//! toolkit makes the graph rules testable without a windowing backend.

use pw_graph_core::{Direction, Graph, LinkId, NodeId, NodeType, Port, PortId, PortType};
use std::collections::{BTreeMap, BTreeSet, HashMap};

pub use pw_graph_core::NodeAppearance;

/// Local UI mirror of a node's audio controls.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NodeAudioState {
    pub muted: bool,
    pub volume: f32,
}

impl Default for NodeAudioState {
    fn default() -> Self {
        Self {
            muted: false,
            volume: 1.0,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum CanvasAction {
    Connect {
        output: PortId,
        input: PortId,
    },
    ConnectMany {
        pairs: Vec<(PortId, PortId)>,
    },
    Disconnect {
        link: LinkId,
    },
    DisconnectMany {
        links: Vec<LinkId>,
    },
    DisconnectNode {
        node: NodeId,
    },
    RemoveEffect {
        node: NodeId,
    },
    ArrangeNodes {
        nodes: Vec<NodeId>,
    },
    SetNodeAppearance {
        node: NodeId,
        appearance: NodeAppearance,
    },
    SetNodeMute {
        node: NodeId,
        muted: bool,
    },
    SetNodeVolume {
        node: NodeId,
        volume: f32,
    },
    SetEffectEnabled {
        node: NodeId,
        enabled: bool,
    },
    SetEffectParameter {
        node: NodeId,
        parameter: String,
        value: f32,
    },
    MoveNode {
        node: NodeId,
        position: [f32; 2],
    },
    CommitNodeMove {
        before: Vec<(NodeId, [f32; 2])>,
        after: Vec<(NodeId, [f32; 2])>,
    },
}

pw_graph_utils::enum_str! {
    #[derive(Default)]
    pub enum ConnectMode {
        #[default]
        Advanced = "advanced",
        Easy = "easy",
    }
}

pw_graph_utils::enum_str! {
    #[derive(Default)]
    pub enum MediaFilter {
        #[default]
        All = "all",
        Audio = "audio",
        Video = "video",
        Midi = "midi",
    }
}

impl MediaFilter {
    pub fn matches_port_type(self, port_type: PortType) -> bool {
        match self {
            Self::All => true,
            Self::Audio => port_type == PortType::Audio,
            Self::Video => port_type == PortType::Video,
            Self::Midi => matches!(port_type, PortType::MidiJack | PortType::MidiAlsa),
        }
    }

    pub fn matches_node(self, graph: &Graph, node_id: NodeId) -> bool {
        let Some(node) = graph.node(node_id) else {
            return false;
        };
        self == Self::All
            || node.ports.iter().any(|port_id| {
                graph
                    .port(*port_id)
                    .is_some_and(|port| self.matches_port_type(port.port_type))
            })
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MeterReading {
    pub rms: f32,
    pub peak: f32,
    pub age_ms: u32,
    pub available: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EffectNodeParameter {
    pub id: String,
    pub name: String,
    pub minimum: f32,
    pub maximum: f32,
    pub value: f32,
    pub unit: String,
    pub boolean: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EffectNodeControl {
    pub enabled: bool,
    pub parameters: Vec<EffectNodeParameter>,
}

/// Safe mapping between the backend's opaque 64-bit IDs and Slint's `int`
/// fields. IDs are allocated densely and never cast, so large PipeWire IDs
/// cannot wrap or collide in the UI model.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SlintIdMap {
    next: i32,
    nodes: BTreeMap<NodeId, i32>,
    ports: BTreeMap<PortId, i32>,
    links: BTreeMap<LinkId, i32>,
}

impl SlintIdMap {
    pub fn rebuild(&mut self, graph: &Graph) {
        self.nodes.retain(|id, _| graph.nodes.contains_key(id));
        self.ports.retain(|id, _| graph.ports.contains_key(id));
        self.links.retain(|id, _| graph.links.contains_key(id));
        for id in graph.nodes.keys() {
            self.alloc_node(*id);
        }
        for id in graph.ports.keys() {
            self.alloc_port(*id);
        }
        for id in graph.links.keys() {
            self.alloc_link(*id);
        }
    }

    pub fn node(&self, id: NodeId) -> Option<i32> {
        self.nodes.get(&id).copied()
    }

    pub fn port(&self, id: PortId) -> Option<i32> {
        self.ports.get(&id).copied()
    }

    pub fn link(&self, id: LinkId) -> Option<i32> {
        self.links.get(&id).copied()
    }

    pub fn node_id(&self, id: i32) -> Option<NodeId> {
        self.nodes
            .iter()
            .find_map(|(source, view)| (*view == id).then_some(*source))
    }

    pub fn port_id(&self, id: i32) -> Option<PortId> {
        self.ports
            .iter()
            .find_map(|(source, view)| (*view == id).then_some(*source))
    }

    pub fn link_id(&self, id: i32) -> Option<LinkId> {
        self.links
            .iter()
            .find_map(|(source, view)| (*view == id).then_some(*source))
    }

    fn alloc_node(&mut self, id: NodeId) -> i32 {
        if let Some(value) = self.nodes.get(&id) {
            return *value;
        }
        let value = self.alloc();
        self.nodes.insert(id, value);
        value
    }

    fn alloc_port(&mut self, id: PortId) -> i32 {
        if let Some(value) = self.ports.get(&id) {
            return *value;
        }
        let value = self.alloc();
        self.ports.insert(id, value);
        value
    }

    fn alloc_link(&mut self, id: LinkId) -> i32 {
        if let Some(value) = self.links.get(&id) {
            return *value;
        }
        let value = self.alloc();
        self.links.insert(id, value);
        value
    }

    fn alloc(&mut self) -> i32 {
        self.next = self
            .next
            .checked_add(1)
            .expect("Slint ID map exhausted the signed 32-bit ID space");
        self.next
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct PortGroupView {
    pub pin_id: i32,
    pub ports: Vec<PortId>,
    pub label: String,
    pub direction: Direction,
    pub port_type: PortType,
}

#[derive(Clone, Debug, PartialEq)]
pub struct NodeView {
    pub id: i32,
    pub node_id: NodeId,
    pub title: String,
    pub node_type: NodeType,
    pub position: [f32; 2],
    pub selected: bool,
    pub appearance: NodeAppearance,
    pub ports: Vec<PortGroupView>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LinkView {
    pub id: i32,
    pub link_id: LinkId,
    pub start_pin_id: i32,
    pub end_pin_id: i32,
    pub color: [u8; 4],
    pub selected: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct GraphViewSnapshot {
    pub nodes: Vec<NodeView>,
    pub links: Vec<LinkView>,
    pub ids: SlintIdMap,
}

/// State shared by the Slint bridge and the application command handlers.
pub struct GraphViewState {
    pub zoom: f32,
    pub node_text_scale: f32,
    pub pan: [f32; 2],
    pub sort_ports_by_name: bool,
    pub sort_ports_descending: bool,
    pub thumbnail_mode: bool,
    pub minimap_visible: bool,
    pub repel_overlapping_nodes: bool,
    pub connect_through_nodes: bool,
    pub connect_mode: ConnectMode,
    pub media_filter: MediaFilter,
    pub search_query: String,
    pub meters: BTreeMap<NodeId, MeterReading>,
    pub port_meters: BTreeMap<PortId, MeterReading>,
    pub effect_controls: BTreeMap<NodeId, EffectNodeControl>,
    pub pinned_meter: Option<PortId>,
    pub metering_disabled: bool,
    pub selected_node: Option<NodeId>,
    pub selected_nodes: BTreeSet<NodeId>,
    pub selected_link: Option<LinkId>,
    node_appearances: BTreeMap<NodeId, NodeAppearance>,
    node_audio: BTreeMap<NodeId, NodeAudioState>,
    pub ids: SlintIdMap,
}

/// Compatibility alias for code that still calls the graph presentation a
/// canvas. It no longer contains toolkit-specific rendering types or logic.
pub type GraphCanvas = GraphViewState;

impl Default for GraphViewState {
    fn default() -> Self {
        Self {
            zoom: 1.0,
            node_text_scale: 1.0,
            pan: [24.0, 24.0],
            sort_ports_by_name: true,
            sort_ports_descending: false,
            thumbnail_mode: false,
            minimap_visible: false,
            repel_overlapping_nodes: false,
            connect_through_nodes: false,
            connect_mode: ConnectMode::Advanced,
            media_filter: MediaFilter::All,
            search_query: String::new(),
            meters: BTreeMap::new(),
            port_meters: BTreeMap::new(),
            effect_controls: BTreeMap::new(),
            pinned_meter: None,
            metering_disabled: false,
            selected_node: None,
            selected_nodes: BTreeSet::new(),
            selected_link: None,
            node_appearances: BTreeMap::new(),
            node_audio: BTreeMap::new(),
            ids: SlintIdMap::default(),
        }
    }
}

impl GraphViewState {
    pub fn node_appearance(&self, node_id: NodeId) -> NodeAppearance {
        self.node_appearances
            .get(&node_id)
            .cloned()
            .unwrap_or_default()
    }

    pub fn set_node_appearance(&mut self, node_id: NodeId, appearance: NodeAppearance) {
        if appearance == NodeAppearance::default() {
            self.node_appearances.remove(&node_id);
        } else {
            self.node_appearances.insert(node_id, appearance);
        }
    }

    pub fn node_audio_state(&self, node_id: NodeId) -> NodeAudioState {
        self.node_audio.get(&node_id).copied().unwrap_or_default()
    }

    pub fn set_node_audio_state(&mut self, node_id: NodeId, state: NodeAudioState) {
        if state == NodeAudioState::default() {
            self.node_audio.remove(&node_id);
        } else {
            self.node_audio.insert(node_id, state);
        }
    }

    pub fn set_effect_controls(&mut self, controls: BTreeMap<NodeId, EffectNodeControl>) {
        self.effect_controls = controls;
    }

    pub fn clear_selected_link(&mut self) {
        self.selected_link = None;
    }

    pub fn visible_node_ids(&self, graph: &Graph) -> BTreeSet<NodeId> {
        graph
            .nodes
            .values()
            .filter(|node| {
                self.media_filter.matches_node(graph, node.id)
                    && (self.search_query.trim().is_empty()
                        || node
                            .name
                            .to_ascii_lowercase()
                            .contains(&self.search_query.to_ascii_lowercase())
                        || node.ports.iter().any(|port_id| {
                            graph.port(*port_id).is_some_and(|port| {
                                port.name
                                    .to_ascii_lowercase()
                                    .contains(&self.search_query.to_ascii_lowercase())
                            })
                        }))
            })
            .map(|node| node.id)
            .collect()
    }

    pub fn selected_links(&self, graph: &Graph) -> Vec<LinkId> {
        let Some(selected) = self.selected_link else {
            return Vec::new();
        };
        let Some(selected_link) = graph.link(selected) else {
            return Vec::new();
        };
        if self.connect_mode != ConnectMode::Easy {
            return vec![selected];
        }
        let (Some(source), Some(destination)) = (
            graph.port(selected_link.output_port),
            graph.port(selected_link.input_port),
        ) else {
            return vec![selected];
        };
        graph
            .links
            .values()
            .filter(|link| {
                let (Some(output), Some(input)) =
                    (graph.port(link.output_port), graph.port(link.input_port))
                else {
                    return false;
                };
                output.node_id == source.node_id
                    && input.node_id == destination.node_id
                    && output.port_type == source.port_type
                    && input.port_type == destination.port_type
            })
            .map(|link| link.id)
            .collect()
    }

    pub fn requested_meter_nodes(&self, graph: &Graph) -> BTreeSet<NodeId> {
        let pinned = self
            .pinned_meter
            .and_then(|port_id| graph.port(port_id))
            .map(|port| port.node_id);
        self.visible_node_ids(graph)
            .into_iter()
            .filter(|node_id| {
                graph.node(*node_id).is_some_and(|node| {
                    node.ports.iter().any(|port_id| {
                        graph
                            .port(*port_id)
                            .is_some_and(|port| port.port_type == PortType::Audio)
                    })
                })
            })
            .chain(pinned)
            .collect()
    }

    pub fn snapshot(&mut self, graph: &Graph) -> GraphViewSnapshot {
        self.ids.rebuild(graph);
        let visible = self.visible_node_ids(graph);
        self.selected_nodes.retain(|id| visible.contains(id));
        if self.selected_node.is_some_and(|id| !visible.contains(&id)) {
            self.selected_node = self.selected_nodes.iter().next().copied();
        }

        let mut nodes = Vec::new();
        let mut pin_groups = HashMap::<PortId, i32>::new();
        for node in graph
            .nodes
            .values()
            .filter(|node| visible.contains(&node.id))
        {
            let mut ports: Vec<&Port> = node
                .ports
                .iter()
                .filter_map(|id| graph.port(*id))
                .filter(|port| {
                    self.media_filter.matches_port_type(port.port_type)
                        && (self.search_query.trim().is_empty()
                            || port
                                .name
                                .to_ascii_lowercase()
                                .contains(&self.search_query.to_ascii_lowercase()))
                })
                .collect();
            if self.sort_ports_by_name {
                ports.sort_by(|a, b| {
                    a.name
                        .to_ascii_lowercase()
                        .cmp(&b.name.to_ascii_lowercase())
                });
            } else {
                ports.sort_by_key(|port| port.id);
            }
            if self.sort_ports_descending {
                ports.reverse();
            }
            let mut groups: Vec<PortGroupView> = Vec::new();
            let mut group_keys = HashMap::<(Direction, PortType, String), usize>::new();
            for port in ports {
                let key = if self.connect_mode == ConnectMode::Easy
                    && port.port_type == PortType::Audio
                {
                    channel_group_key(port)
                        .map(|base| (port.direction, port.port_type, base.to_ascii_lowercase()))
                } else {
                    None
                };
                let group_index = key.as_ref().and_then(|key| group_keys.get(key).copied());
                if let Some(index) = group_index {
                    groups[index].ports.push(port.id);
                    if groups[index].ports.len() == 2 {
                        if let Some(base) = channel_base_name(port).filter(|base| !base.is_empty())
                        {
                            groups[index].label = base;
                        }
                    }
                    pin_groups.insert(port.id, groups[index].pin_id);
                } else {
                    let pin_id = self.ids.port(port.id).unwrap_or(0);
                    let index = groups.len();
                    groups.push(PortGroupView {
                        pin_id,
                        ports: vec![port.id],
                        label: port.name.clone(),
                        direction: port.direction,
                        port_type: port.port_type,
                    });
                    if let Some(key) = key {
                        group_keys.insert(key, index);
                    }
                    pin_groups.insert(port.id, pin_id);
                }
            }
            nodes.push(NodeView {
                id: self.ids.node(node.id).unwrap_or(0),
                node_id: node.id,
                title: self
                    .node_appearance(node.id)
                    .custom_name
                    .unwrap_or_else(|| node.name.clone()),
                node_type: node.node_type,
                position: node.position,
                selected: self.selected_nodes.contains(&node.id),
                appearance: self.node_appearance(node.id),
                ports: groups,
            });
        }

        let selected_links = self.selected_links(graph);
        let links = graph
            .links
            .values()
            .filter_map(|link| {
                let output = graph.port(link.output_port)?;
                let input = graph.port(link.input_port)?;
                if !visible.contains(&output.node_id)
                    || !visible.contains(&input.node_id)
                    || !self.media_filter.matches_port_type(output.port_type)
                    || !self.media_filter.matches_port_type(input.port_type)
                {
                    return None;
                }
                Some(LinkView {
                    id: self.ids.link(link.id)?,
                    link_id: link.id,
                    start_pin_id: *pin_groups.get(&link.output_port)?,
                    end_pin_id: *pin_groups.get(&link.input_port)?,
                    color: port_type_color(output.port_type),
                    selected: selected_links.contains(&link.id),
                })
            })
            .collect();

        GraphViewSnapshot {
            nodes,
            links,
            ids: self.ids.clone(),
        }
    }

    pub fn visible_counts(&self, graph: &Graph) -> (usize, usize, usize) {
        let visible = self.visible_node_ids(graph);
        let ports = graph
            .ports
            .values()
            .filter(|port| {
                visible.contains(&port.node_id)
                    && self.media_filter.matches_port_type(port.port_type)
            })
            .count();
        let links = graph
            .links
            .values()
            .filter(|link| {
                graph
                    .port(link.output_port)
                    .is_some_and(|output| visible.contains(&output.node_id))
                    && graph
                        .port(link.input_port)
                        .is_some_and(|input| visible.contains(&input.node_id))
            })
            .count();
        (visible.len(), ports, links)
    }
}

pub fn port_type_color(port_type: PortType) -> [u8; 4] {
    match port_type {
        PortType::Audio => [87, 199, 133, 255],
        PortType::Video => [78, 157, 230, 255],
        PortType::MidiJack => [227, 93, 106, 255],
        PortType::MidiAlsa => [169, 121, 209, 255],
        PortType::Unknown => [165, 165, 165, 255],
    }
}

/// Pair source and sink ports for a group connection. Compatible channel
/// positions win over registry order, while ports without channel metadata
/// retain the stable display order fallback.
pub fn pair_ports(graph: &Graph, first: &[PortId], second: &[PortId]) -> Vec<(PortId, PortId)> {
    let mut outputs = Vec::new();
    let mut inputs = Vec::new();
    for id in first.iter().chain(second.iter()) {
        let Some(port) = graph.port(*id) else {
            continue;
        };
        if port.direction == Direction::Source {
            outputs.push(port);
        } else {
            inputs.push(port);
        }
    }

    let mut used = vec![false; inputs.len()];
    let mut pairs = Vec::new();
    for output in outputs {
        let candidate = inputs
            .iter()
            .enumerate()
            .filter(|(index, input)| {
                !used[*index]
                    && ports_compatible(output.port_type, input.port_type)
                    && channels_can_pair(output, input)
            })
            .max_by_key(|(index, input)| {
                (
                    channel_pair_score(output, input),
                    name_pair_score(output, input),
                    std::cmp::Reverse(*index),
                )
            })
            .map(|(index, _)| index);
        if let Some(index) = candidate {
            used[index] = true;
            pairs.push((output.id, inputs[index].id));
        }
    }
    pairs
}

fn ports_compatible(a: PortType, b: PortType) -> bool {
    a == b || a == PortType::Unknown || b == PortType::Unknown
}

fn channels_can_pair(output: &Port, input: &Port) -> bool {
    match (channel_identity(output), channel_identity(input)) {
        (Some(output), Some(input)) => output == input,
        _ => true,
    }
}

fn channel_pair_score(output: &Port, input: &Port) -> u8 {
    match (channel_identity(output), channel_identity(input)) {
        (Some(output), Some(input)) if output == input => 100,
        (Some(_), Some(_)) => 0,
        (Some(_), None) | (None, Some(_)) => 20,
        (None, None) => 10,
    }
}

fn name_pair_score(output: &Port, input: &Port) -> u8 {
    match (channel_base_name(output), channel_base_name(input)) {
        (Some(output), Some(input))
            if !output.is_empty() && output.eq_ignore_ascii_case(&input) =>
        {
            10
        }
        _ => 0,
    }
}

fn channel_identity(port: &Port) -> Option<String> {
    let raw = port
        .channel
        .as_deref()
        .filter(|channel| is_backend_channel_position(channel))
        .or_else(|| trailing_channel_token(&port.name))?;
    Some(normalize_channel(raw))
}

fn trailing_channel_token(name: &str) -> Option<&str> {
    let position = name.rfind(|character| ['_', '-', ' ', ':', '.'].contains(&character))?;
    let token = &name[position + 1..];
    is_channel_token(token).then_some(token)
}

fn is_backend_channel_position(channel: &str) -> bool {
    let normalized = channel.trim().to_ascii_uppercase();
    !normalized.is_empty()
        && !matches!(normalized.as_str(), "UNKNOWN" | "UNDEFINED" | "NONE" | "NA")
}

fn normalize_channel(channel: &str) -> String {
    let compact: String = channel
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_uppercase)
        .collect();
    match compact.as_str() {
        "FRONTLEFT" | "LEFT" | "L" => "FL".into(),
        "FRONTRIGHT" | "RIGHT" | "R" => "FR".into(),
        "REARLEFT" | "BACKLEFT" => "RL".into(),
        "REARRIGHT" | "BACKRIGHT" => "RR".into(),
        "SIDELEFT" => "SL".into(),
        "SIDERIGHT" => "SR".into(),
        "CENTER" | "FC" | "C" => "C".into(),
        "LOWFREQUENCY" => "LFE".into(),
        _ => compact,
    }
}

fn channel_group_key(port: &Port) -> Option<String> {
    (port.port_type == PortType::Audio).then(|| channel_base_name(port))?
}

fn channel_base_name(port: &Port) -> Option<String> {
    const DELIMITERS: [char; 5] = ['_', '-', ' ', ':', '.'];
    let name = port.name.as_str();

    // PipeWire's audio.channel metadata is authoritative even when the
    // display name uses an ordinal suffix such as `output_1`.
    if port.channel.as_deref().is_some_and(|channel| {
        let normalized = channel.trim().to_ascii_uppercase();
        !normalized.is_empty()
            && !matches!(normalized.as_str(), "UNKNOWN" | "UNDEFINED" | "NONE" | "NA")
    }) {
        if let Some(position) = name.rfind(|character| DELIMITERS.contains(&character)) {
            let (base, _) = name.split_at(position);
            if !base.is_empty() {
                return Some(base.to_owned());
            }
        }
        if is_channel_token(name) {
            return Some(String::new());
        }
        if !name.is_empty() {
            return Some(name.to_owned());
        }
        return None;
    }

    if let Some(position) = name.rfind(|character| DELIMITERS.contains(&character)) {
        let (base, suffix) = name.split_at(position);
        let suffix = suffix.trim_start_matches(DELIMITERS);
        if !base.is_empty() && is_channel_token(suffix) {
            return Some(base.to_owned());
        }
    }
    is_channel_token(name).then_some(String::new())
}

fn is_channel_token(token: &str) -> bool {
    const TOKENS: [&str; 34] = [
        "FL", "FR", "RL", "RR", "SL", "SR", "FC", "RC", "LFE", "MONO", "LEFT", "RIGHT", "L", "R",
        "C", "FLC", "FRC", "TC", "TFL", "TFR", "TFC", "TRL", "TRR", "TRC", "BFL", "BFR", "BFC",
        "BL", "BR", "BC", "BLC", "BRC", "TBL", "TBR",
    ];
    let token = token.trim();
    TOKENS
        .iter()
        .any(|candidate| candidate.eq_ignore_ascii_case(token))
        || token.strip_prefix("AUX").is_some_and(|suffix| {
            !suffix.is_empty() && suffix.chars().all(|character| character.is_ascii_digit())
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pw_graph_core::{Node, Port};

    fn graph() -> Graph {
        let mut graph = Graph::default();
        graph
            .add_node(Node::new(NodeId(1), "Source", NodeType::PipeWire))
            .unwrap();
        graph
            .add_node(Node::new(NodeId(2), "Sink", NodeType::PipeWire))
            .unwrap();
        graph
            .add_port(Port::new(
                PortId(u64::MAX),
                NodeId(1),
                "out_FL",
                Direction::Source,
                PortType::Audio,
            ))
            .unwrap();
        graph
            .add_port(Port::new(
                PortId(21),
                NodeId(1),
                "out_FR",
                Direction::Source,
                PortType::Audio,
            ))
            .unwrap();
        graph
            .add_port(Port::new(
                PortId(30),
                NodeId(2),
                "in_FL",
                Direction::Sink,
                PortType::Audio,
            ))
            .unwrap();
        graph
            .add_port(Port::new(
                PortId(31),
                NodeId(2),
                "in_FR",
                Direction::Sink,
                PortType::Audio,
            ))
            .unwrap();
        graph
            .add_link(LinkId(9), PortId(u64::MAX), PortId(30))
            .unwrap();
        graph.add_link(LinkId(10), PortId(21), PortId(31)).unwrap();
        graph
    }

    #[test]
    fn ids_round_trip_without_casting_u64() {
        let graph = graph();
        let mut ids = SlintIdMap::default();
        ids.rebuild(&graph);
        let port = ids.port(PortId(u64::MAX)).unwrap();
        assert_eq!(ids.port_id(port), Some(PortId(u64::MAX)));
        assert_ne!(port as u64, u64::MAX);
    }

    #[test]
    fn easy_snapshot_groups_channels_and_selects_matching_links() {
        let graph = graph();
        let mut view = GraphViewState {
            connect_mode: ConnectMode::Easy,
            selected_link: Some(LinkId(9)),
            ..GraphViewState::default()
        };
        let snapshot = view.snapshot(&graph);
        assert_eq!(snapshot.nodes[0].ports.len(), 1);
        assert_eq!(view.selected_links(&graph), vec![LinkId(9), LinkId(10)]);
        assert_eq!(snapshot.links.len(), 2);
        assert_eq!(
            pair_ports(
                &graph,
                &[PortId(21), PortId(u64::MAX)],
                &[PortId(30), PortId(31)]
            ),
            vec![(PortId(21), PortId(31)), (PortId(u64::MAX), PortId(30))]
        );
    }
}
