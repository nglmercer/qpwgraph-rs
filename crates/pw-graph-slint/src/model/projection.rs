//! Turning graph facts into pixels and colours: node geometry, the palette
//! each node and port type is drawn with, and the stored positions and
//! appearances a projection starts from.

use super::*;

pub(crate) fn node_type_color(node_type: NodeType) -> [u8; 4] {
    match node_type {
        NodeType::PipeWire => [63, 82, 101, 255],
        NodeType::Effect => [82, 117, 176, 255],
        NodeType::AlsaMidi => [138, 93, 159, 255],
        NodeType::WindowsAudioEndpoint => [49, 129, 143, 255],
        NodeType::WindowsAudioSession => [64, 157, 168, 255],
        NodeType::WindowsMidi => [155, 105, 174, 255],
        NodeType::Unknown => [112, 112, 112, 255],
    }
}

/// The canvas palette distinguishes input, output, and monitor ports within
/// each media family. Keeping the role calculation here means Slint and the
/// hit-tested graph model use the same colors for dots, accents, and links.
pub(crate) fn port_color(port_type: PortType, direction: Direction, name: &str) -> [u8; 4] {
    let monitor = name_contains_monitor(name);
    match (port_type, monitor, direction) {
        (PortType::Audio, true, _) => [139, 231, 177, 255],
        (PortType::Audio, false, Direction::Sink) => [44, 151, 96, 255],
        (PortType::Audio, false, Direction::Source) => [82, 207, 133, 255],
        (PortType::Video, true, _) => [151, 213, 255, 255],
        (PortType::Video, false, Direction::Sink) => [43, 125, 202, 255],
        (PortType::Video, false, Direction::Source) => [91, 181, 244, 255],
        (PortType::MidiJack, true, _) => [255, 161, 177, 255],
        (PortType::MidiJack, false, Direction::Sink) => [186, 57, 87, 255],
        (PortType::MidiJack, false, Direction::Source) => [237, 108, 128, 255],
        (PortType::MidiAlsa, true, _) => [220, 177, 245, 255],
        (PortType::MidiAlsa, false, Direction::Sink) => [128, 78, 172, 255],
        (PortType::MidiAlsa, false, Direction::Source) => [190, 132, 225, 255],
        (PortType::Unknown, true, _) => [214, 222, 232, 255],
        (PortType::Unknown, false, Direction::Sink) => [116, 127, 141, 255],
        (PortType::Unknown, false, Direction::Source) => [177, 188, 202, 255],
    }
}

pub(crate) fn link_color(port_type: PortType, direction: Direction, name: &str) -> [u8; 4] {
    let monitor = name_contains_monitor(name);
    match (port_type, monitor, direction) {
        (PortType::Audio, true, _) => [105, 194, 145, 255],
        (PortType::Audio, false, Direction::Sink) => [38, 126, 80, 255],
        (PortType::Audio, false, Direction::Source) => [62, 173, 109, 255],
        (PortType::Video, true, _) => [112, 177, 218, 255],
        (PortType::Video, false, Direction::Sink) => [37, 105, 170, 255],
        (PortType::Video, false, Direction::Source) => [69, 147, 204, 255],
        (PortType::MidiJack, true, _) => [220, 125, 145, 255],
        (PortType::MidiJack, false, Direction::Sink) => [151, 48, 72, 255],
        (PortType::MidiJack, false, Direction::Source) => [198, 83, 105, 255],
        (PortType::MidiAlsa, true, _) => [187, 145, 213, 255],
        (PortType::MidiAlsa, false, Direction::Sink) => [104, 62, 141, 255],
        (PortType::MidiAlsa, false, Direction::Source) => [157, 105, 191, 255],
        (PortType::Unknown, true, _) => [178, 188, 202, 255],
        (PortType::Unknown, false, Direction::Sink) => [93, 103, 116, 255],
        (PortType::Unknown, false, Direction::Source) => [143, 155, 171, 255],
    }
}

pub(super) fn node_height(
    thumbnail: bool,
    collapsed: bool,
    has_audio_panel: bool,
    port_count: usize,
) -> f32 {
    if thumbnail {
        62.0
    } else if collapsed {
        COLLAPSED_NODE_HEIGHT
    } else {
        NODE_HEADER_HEIGHT
            + if has_audio_panel {
                AUDIO_CONTROLS_HEIGHT
            } else {
                0.0
            }
            + 14.0
            + port_count as f32 * PORT_ROW_HEIGHT
    }
}

pub(super) fn intersects(
    position: [f32; 2],
    size: [f32; 2],
    x: f32,
    y: f32,
    w: f32,
    h: f32,
) -> bool {
    position[0] < x + w
        && position[0] + size[0] > x
        && position[1] < y + h
        && position[1] + size[1] > y
}

pub(super) fn point_in_box(point: (f32, f32), x: f32, y: f32, width: f32, height: f32) -> bool {
    point.0 >= x && point.0 <= x + width && point.1 >= y && point.1 <= y + height
}

pub(super) fn configured_positions(
    graph: &Graph,
    config: &AppConfig,
) -> BTreeMap<NodeId, [f32; 2]> {
    let defaults = graph.default_node_positions();
    let mut key_counts = BTreeMap::<String, usize>::new();
    let mut legacy_key_counts = BTreeMap::<String, usize>::new();
    for node in graph.nodes.values() {
        *key_counts.entry(node_layout_key(node)).or_default() += 1;
        *legacy_key_counts
            .entry(node_layout_legacy_key(node))
            .or_default() += 1;
    }
    graph
        .nodes
        .values()
        .map(|node| {
            let key = node_layout_key(node);
            let legacy_key = node_layout_legacy_key(node);
            let by_id = config.node_positions.get(&node.id.0.to_string()).copied();
            let by_name = (key_counts.get(&key) == Some(&1))
                .then(|| {
                    config
                        .node_positions_by_name
                        .get(&key)
                        .copied()
                        .or_else(|| {
                            (legacy_key_counts.get(&legacy_key) == Some(&1))
                                .then(|| config.node_positions_by_name.get(&legacy_key).copied())
                                .flatten()
                        })
                })
                .flatten();
            (
                node.id,
                by_id
                    .or(by_name)
                    .or_else(|| defaults.get(&node.id).copied())
                    .unwrap_or(node.position),
            )
        })
        .collect()
}

pub(super) fn configured_appearances(
    graph: &Graph,
    config: &AppConfig,
) -> BTreeMap<NodeId, NodeAppearance> {
    let mut key_counts = BTreeMap::<String, usize>::new();
    let mut legacy_key_counts = BTreeMap::<String, usize>::new();
    for node in graph.nodes.values() {
        *key_counts.entry(node_layout_key(node)).or_default() += 1;
        *legacy_key_counts
            .entry(node_layout_legacy_key(node))
            .or_default() += 1;
    }
    graph
        .nodes
        .values()
        .filter_map(|node| {
            let key = node_layout_key(node);
            let legacy_key = node_layout_legacy_key(node);
            (key_counts.get(&key) == Some(&1)).then(|| {
                (
                    node.id,
                    config
                        .node_view_by_name
                        .get(&key)
                        .cloned()
                        .or_else(|| {
                            (legacy_key_counts.get(&legacy_key) == Some(&1))
                                .then(|| config.node_view_by_name.get(&legacy_key).cloned())
                                .flatten()
                        })
                        .unwrap_or_default(),
                )
            })
        })
        .collect()
}

pub(crate) fn is_relay_node(node: &Node) -> bool {
    matches!(node.name.as_str(), RELAY_SOURCE_NAME | RELAY_SINK_NAME)
}

/// Case-insensitive substring search for "monitor" without allocating a
/// lowercased copy of the port name on every projection.
fn name_contains_monitor(name: &str) -> bool {
    const NEEDLE: &[u8] = b"monitor";
    let bytes = name.as_bytes();
    if bytes.len() < NEEDLE.len() {
        return false;
    }
    bytes
        .windows(NEEDLE.len())
        .any(|window| window.eq_ignore_ascii_case(NEEDLE))
}
