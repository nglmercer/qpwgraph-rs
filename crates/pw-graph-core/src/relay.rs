//! Core graph types shared by every backend and presentation layer.

use super::endpoint::Direction;

/// PipeWire node name of the relay's virtual capture device, presented to the
/// user as "Relay Microphone". Defined here so the backend that creates the
/// node, the UI that renames it, and the patchbay compatibility rules below
/// all agree on one spelling.
pub const RELAY_SOURCE_NODE_NAME: &str = "qpwgraph-rs.relay.source";
/// PipeWire node name of the relay's virtual playback device, presented as
/// "Relay Speaker".
pub const RELAY_SINK_NODE_NAME: &str = "qpwgraph-rs.relay.sink";
/// Role prefix the relay source node gives its ports (`capture_FL`, ...).
pub const RELAY_SOURCE_PORT_ROLE: &str = "capture";
/// Role prefix the relay sink node gives its ports (`playback_FL`, ...).
pub const RELAY_SINK_PORT_ROLE: &str = "playback";

/// The port name a pre-rename patchbay entry should resolve to, if it is one.
///
/// The relay filter ports were once named for their bare channel — `FL` and
/// `FR` — which left them as two loose pins with no base for the canvas to
/// group on. They are now role-prefixed like every other node's ports, but
/// patchbay files saved before that rename still carry the bare names and
/// would otherwise silently stop reconnecting.
///
/// This is deliberately narrow: it fires only for the two relay node names,
/// only for the two bare channel names, and only in the direction that node
/// actually has ports in. An unrelated device with `FL`/`FR` ports is never
/// rewritten, and nothing here affects how new patchbays are written.
pub fn legacy_relay_port_name(
    node_name: &str,
    port_name: &str,
    direction: Direction,
) -> Option<&'static str> {
    if !matches!(port_name, "FL" | "FR") {
        return None;
    }
    match (node_name, direction) {
        (RELAY_SOURCE_NODE_NAME, Direction::Source) => match port_name {
            "FL" => Some("capture_FL"),
            _ => Some("capture_FR"),
        },
        (RELAY_SINK_NODE_NAME, Direction::Sink) => match port_name {
            "FL" => Some("playback_FL"),
            _ => Some("playback_FR"),
        },
        _ => None,
    }
}
