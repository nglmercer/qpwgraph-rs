//! Geometry and hit-testing for the node canvas.
//!
//! The canvas is deliberately split so that Rust owns every coordinate: the
//! Slint side only renders what this module describes and reports raw pointer
//! gestures back. Nothing in the UI has to measure itself and report positions
//! back to Rust, which is what used to make dragging and edge creation
//! disagree about where a pin actually is.

use std::collections::{BTreeMap, HashMap, HashSet};

/// Visual height of the node header (the drag handle).
pub(crate) const HEADER_HEIGHT: f32 = 40.0;
/// Top of the node body, just below the header border.
pub(crate) const BODY_TOP: f32 = 41.0;
/// Space reserved inside the body for the audio controls block.
pub(crate) const AUDIO_BLOCK_HEIGHT: f32 = 45.0;
/// Padding above the first port row when there are no audio controls.
pub(crate) const PORT_LIST_TOP: f32 = 5.0;
/// Vertical pitch between two port rows.
pub(crate) const PORT_ROW_PITCH: f32 = 25.0;
/// Drawn height of a single port row.
pub(crate) const PORT_ROW_HEIGHT: f32 = 22.0;
/// Horizontal distance from the node edge to the centre of a pin.
pub(crate) const PIN_INSET: f32 = 10.0;
/// Pointer slack around a pin, in logical screen pixels.
///
/// Hit tolerances are screen-space on purpose: they describe how precisely a
/// person can aim, which does not change when the canvas is zoomed out. They
/// are divided by the zoom just before they are used against world geometry.
pub(crate) const PIN_SCREEN_HIT_RADIUS: f32 = 11.0;
/// Pointer slack around a link, in logical screen pixels.
pub(crate) const LINK_SCREEN_HIT_RADIUS: f32 = 9.0;
/// Snap distance for resolving a finished drop onto a pin, in world units.
///
/// Unlike the pointer tolerances above this one is about card geometry -- how
/// close to a port a drop counts as landing on it -- so it stays world-space.
pub(crate) const PIN_DROP_RADIUS: f32 = 11.0;

/// Nothing was hit.
pub(crate) const HIT_NONE: i32 = 0;
/// A pin was hit; `id` is the pin id and `x`/`y` its world centre.
pub(crate) const HIT_PIN: i32 = 1;
/// A node was hit somewhere that starts a move gesture.
pub(crate) const HIT_NODE: i32 = 2;
/// A link was hit; `id` is the link id.
pub(crate) const HIT_LINK: i32 = 3;
/// A node body was hit in Easy mode, which starts a whole-node connection.
pub(crate) const HIT_NODE_BODY: i32 = 4;

/// Position of the pin dot inside a node, relative to the node origin.
pub(crate) fn pin_offset(
    node_width: f32,
    index: usize,
    has_audio_controls: bool,
    is_output: bool,
) -> (f32, f32) {
    let x = if is_output {
        node_width - PIN_INSET
    } else {
        PIN_INSET
    };
    (
        x,
        port_row_top(index, has_audio_controls) + PORT_ROW_HEIGHT / 2.0,
    )
}

/// Top edge of a port row inside a node, relative to the node origin.
pub(crate) fn port_row_top(index: usize, has_audio_controls: bool) -> f32 {
    BODY_TOP
        + if has_audio_controls {
            AUDIO_BLOCK_HEIGHT
        } else {
            PORT_LIST_TOP
        }
        + index as f32 * PORT_ROW_PITCH
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct NodeGeometry {
    pub(crate) id: i32,
    pub(crate) x: f32,
    pub(crate) y: f32,
    pub(crate) width: f32,
    pub(crate) height: f32,
    pub(crate) selected: bool,
    /// Collapsed and thumbnail cards draw no pins; their edges act as anchors.
    pub(crate) pins_visible: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PinGeometry {
    pub(crate) pin_id: i32,
    pub(crate) node_id: i32,
    /// `true` for source ports, which anchor curves to the right.
    pub(crate) is_output: bool,
    pub(crate) x: f32,
    pub(crate) y: f32,
    pub(crate) visible: bool,
    pub(crate) node_selected: bool,
    /// Whether this pin''s backend can actually make a connection. Backend-wide
    /// capability is a union across children, so a Windows audio pin and a
    /// Windows MIDI pin disagree even though the composite says `connect`.
    pub(crate) connectable: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct LinkGeometry {
    pub(crate) id: i32,
    pub(crate) start_pin: i32,
    pub(crate) end_pin: i32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Hit {
    pub(crate) kind: i32,
    pub(crate) id: i32,
    pub(crate) x: f32,
    pub(crate) y: f32,
}

impl Hit {
    fn none() -> Self {
        Self {
            kind: HIT_NONE,
            ..Self::default()
        }
    }
}

/// The world-space picture of the graph, rebuilt whenever the model syncs.
#[derive(Clone, Debug, Default)]
pub(crate) struct CanvasGeometry {
    nodes: Vec<NodeGeometry>,
    node_index: HashMap<i32, usize>,
    pins: BTreeMap<i32, PinGeometry>,
    links: Vec<LinkGeometry>,
    link_index: HashMap<i32, usize>,
    easy_mode: bool,
}

impl CanvasGeometry {
    pub(crate) fn replace(
        &mut self,
        nodes: Vec<NodeGeometry>,
        pins: Vec<PinGeometry>,
        links: Vec<LinkGeometry>,
        easy_mode: bool,
    ) {
        self.nodes = nodes;
        self.pins = pins.into_iter().map(|pin| (pin.pin_id, pin)).collect();
        self.links = links;
        self.easy_mode = easy_mode;
        self.rebuild_indices();
    }

    fn rebuild_indices(&mut self) {
        self.node_index = self
            .nodes
            .iter()
            .enumerate()
            .map(|(index, node)| (node.id, index))
            .collect();
        self.link_index = self
            .links
            .iter()
            .enumerate()
            .map(|(index, link)| (link.id, index))
            .collect();
    }

    fn link(&self, id: i32) -> Option<LinkGeometry> {
        self.link_index
            .get(&id)
            .copied()
            .and_then(|index| self.links.get(index).copied())
    }

    /// Mirror the selection flags of the rendered rows, so a drag started in
    /// the same event that changed the selection already moves the right cards.
    pub(crate) fn apply_selection(&mut self, is_selected: impl Fn(i32) -> bool) {
        for node in &mut self.nodes {
            node.selected = is_selected(node.id);
        }
        for pin in self.pins.values_mut() {
            pin.node_selected = is_selected(pin.node_id);
        }
    }

    /// Move the committed drag into the cache so the edges stay attached until
    /// the next model sync rebuilds it.
    pub(crate) fn translate_selected(&mut self, dragged: i32, dx: f32, dy: f32) {
        let moved: HashSet<i32> = self
            .nodes
            .iter()
            .filter(|node| node.selected || node.id == dragged)
            .map(|node| node.id)
            .collect();
        for node in &mut self.nodes {
            if moved.contains(&node.id) {
                node.x += dx;
                node.y += dy;
            }
        }
        for pin in self.pins.values_mut() {
            if moved.contains(&pin.node_id) {
                pin.x += dx;
                pin.y += dy;
            }
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub(crate) fn node(&self, id: i32) -> Option<NodeGeometry> {
        self.node_index
            .get(&id)
            .copied()
            .and_then(|index| self.nodes.get(index).copied())
    }

    pub(crate) fn pin(&self, id: i32) -> Option<PinGeometry> {
        self.pins.get(&id).copied()
    }

    /// Anchor point of a pin, falling back to the node edge for cards that
    /// draw no pins (collapsed or thumbnail).
    fn anchor(&self, pin: &PinGeometry, drag: (f32, f32)) -> (f32, f32) {
        let (mut x, mut y) = if pin.visible {
            (pin.x, pin.y)
        } else {
            match self.node(pin.node_id) {
                Some(node) => (
                    if pin.is_output {
                        node.x + node.width
                    } else {
                        node.x
                    },
                    node.y + HEADER_HEIGHT / 2.0,
                ),
                None => (pin.x, pin.y),
            }
        };
        if pin.node_selected {
            x += drag.0;
            y += drag.1;
        }
        (x, y)
    }

    /// Whether a pin belongs to a backend that can rewire it.
    pub(crate) fn pin_connectable(&self, pin_id: i32) -> bool {
        self.pin(pin_id).is_some_and(|pin| pin.connectable)
    }

    /// Nearest visible pin within `radius`, or `0` when the pointer is clear of
    /// every pin. `0` is never a valid pin id in the bridge's id space.
    pub(crate) fn find_pin_at(&self, x: f32, y: f32, radius: f32) -> i32 {
        let mut best = (radius * radius, 0);
        for pin in self.pins.values().filter(|pin| pin.visible) {
            let distance = (pin.x - x).powi(2) + (pin.y - y).powi(2);
            if distance <= best.0 {
                best = (distance, pin.pin_id);
            }
        }
        best.1
    }

    /// Topmost node containing the point, or `None`.
    pub(crate) fn find_node_at(&self, x: f32, y: f32) -> Option<NodeGeometry> {
        self.nodes
            .iter()
            .rev()
            .copied()
            .find(|node| contains(node, x, y))
    }

    /// Nearest link within `radius`, or `-1`.
    ///
    /// The curve is flattened into short segments and the pointer is measured
    /// against those segments rather than against the flattening points alone.
    /// Sampling only the points left blind gaps roughly a segment long between
    /// them, so clicking a visibly drawn edge frequently missed it.
    pub(crate) fn find_link_at(&self, x: f32, y: f32, radius: f32) -> i32 {
        const SEGMENTS: usize = 32;
        let mut best = (radius * radius, -1);
        for link in &self.links {
            let (Some(start), Some(end)) = (self.pin(link.start_pin), self.pin(link.end_pin))
            else {
                continue;
            };
            let start = self.anchor(&start, (0.0, 0.0));
            let end = self.anchor(&end, (0.0, 0.0));
            let curve = bezier(start, end);
            // Cheap bounding-box reject before flattening: the curve is fully
            // contained in the hull of its four control points, so a pointer
            // outside that hull (expanded by the hit radius) cannot hit it.
            // On dense graphs this skips the 32-segment test for almost every
            // link on every mouse move.
            let min_x = curve
                .iter()
                .map(|point| point.0)
                .fold(f32::INFINITY, f32::min)
                - radius;
            let max_x = curve
                .iter()
                .map(|point| point.0)
                .fold(f32::NEG_INFINITY, f32::max)
                + radius;
            if x < min_x || x > max_x {
                continue;
            }
            let min_y = curve
                .iter()
                .map(|point| point.1)
                .fold(f32::INFINITY, f32::min)
                - radius;
            let max_y = curve
                .iter()
                .map(|point| point.1)
                .fold(f32::NEG_INFINITY, f32::max)
                + radius;
            if y < min_y || y > max_y {
                continue;
            }
            let mut previous = cubic_at(&curve, 0.0);
            for step in 1..=SEGMENTS {
                let current = cubic_at(&curve, step as f32 / SEGMENTS as f32);
                let distance = squared_distance_to_segment((x, y), previous, current);
                if distance <= best.0 {
                    best = (distance, link.id);
                }
                previous = current;
            }
        }
        best.1
    }

    /// Resolve a pointer press into the gesture it should start.
    ///
    /// `zoom` converts the screen-space pointer tolerances into the world units
    /// the cached geometry is expressed in, so an edge stays just as easy to
    /// click when the canvas is zoomed out as it is at 1:1.
    pub(crate) fn hit_test(&self, x: f32, y: f32, zoom: f32) -> Hit {
        let zoom = if zoom.is_finite() && zoom > 0.01 {
            zoom
        } else {
            1.0
        };
        let pin = self.find_pin_at(x, y, PIN_SCREEN_HIT_RADIUS / zoom);
        if pin != 0 {
            if let Some(pin) = self.pin(pin) {
                return Hit {
                    kind: HIT_PIN,
                    id: pin.pin_id,
                    x: pin.x,
                    y: pin.y,
                };
            }
        }
        if let Some(node) = self.find_node_at(x, y) {
            // The header is the drag handle in both modes; in Easy mode
            // everything below it connects the whole card. Cards that draw no
            // pins (collapsed, thumbnail) are included: they have no pin to
            // start a connection from, so their body is the only surface left.
            let on_header = y - node.y <= HEADER_HEIGHT;
            let kind = if self.easy_mode && !on_header {
                HIT_NODE_BODY
            } else {
                HIT_NODE
            };
            return Hit {
                kind,
                id: node.id,
                x,
                y,
            };
        }
        let link = self.find_link_at(x, y, LINK_SCREEN_HIT_RADIUS / zoom);
        if link >= 0 {
            return Hit {
                kind: HIT_LINK,
                id: link,
                x,
                y,
            };
        }
        Hit::none()
    }

    pub(crate) fn nodes_in_box(&self, x: f32, y: f32, width: f32, height: f32) -> Vec<i32> {
        self.nodes
            .iter()
            .filter(|node| {
                node.x < x + width
                    && node.x + node.width > x
                    && node.y < y + height
                    && node.y + node.height > y
            })
            .map(|node| node.id)
            .collect()
    }

    pub(crate) fn links_in_box(&self, x: f32, y: f32, width: f32, height: f32) -> Vec<i32> {
        self.links
            .iter()
            .filter(|link| {
                let (Some(start), Some(end)) = (self.pin(link.start_pin), self.pin(link.end_pin))
                else {
                    return false;
                };
                let start = self.anchor(&start, (0.0, 0.0));
                let end = self.anchor(&end, (0.0, 0.0));
                point_in_box(start, x, y, width, height) || point_in_box(end, x, y, width, height)
            })
            .map(|link| link.id)
            .collect()
    }

    /// SVG commands for one link in world coordinates. `drag` is the live
    /// offset applied to every selected node while a move gesture is running.
    pub(crate) fn link_path(&self, link_id: i32, drag: (f32, f32)) -> String {
        let Some(link) = self.link(link_id) else {
            return String::new();
        };
        let (Some(start), Some(end)) = (self.pin(link.start_pin), self.pin(link.end_pin)) else {
            return String::new();
        };
        curve_commands(self.anchor(&start, drag), self.anchor(&end, drag))
    }

    /// Which end of a link stays put when it is dragged from `(x, y)`.
    ///
    /// Grabbing an edge near one endpoint moves that endpoint and pivots around
    /// the other, which is how every patchbay behaves. Returns the pin that
    /// stays anchored, or `0` when the link or its pins are not cached.
    pub(crate) fn link_drag_anchor(&self, link_id: i32, x: f32, y: f32) -> i32 {
        let Some(link) = self.link(link_id) else {
            return 0;
        };
        let (Some(start), Some(end)) = (self.pin(link.start_pin), self.pin(link.end_pin)) else {
            return 0;
        };
        let start_point = self.anchor(&start, (0.0, 0.0));
        let end_point = self.anchor(&end, (0.0, 0.0));
        let to_start = (start_point.0 - x).powi(2) + (start_point.1 - y).powi(2);
        let to_end = (end_point.0 - x).powi(2) + (end_point.1 - y).powi(2);
        // The nearer endpoint is the one being moved, so the far one anchors.
        if to_start <= to_end {
            link.end_pin
        } else {
            link.start_pin
        }
    }

    /// SVG commands for the rubber-band curve drawn while creating a link.
    pub(crate) fn preview_path(&self, start_pin: i32, to_x: f32, to_y: f32) -> String {
        let Some(pin) = self.pin(start_pin) else {
            return String::new();
        };
        // The free end has no direction of its own; mirror the anchored side.
        curve_commands_directed(
            self.anchor(&pin, (0.0, 0.0)),
            pin.is_output,
            (to_x, to_y),
            !pin.is_output,
        )
    }

    /// SVG commands for the rubber band of an Easy-mode whole-node drag.
    pub(crate) fn node_preview_path(&self, node_id: i32, to_x: f32, to_y: f32) -> String {
        let Some(node) = self.node(node_id) else {
            return String::new();
        };
        let from_right = to_x >= node.x + node.width / 2.0;
        let start = (
            if from_right {
                node.x + node.width
            } else {
                node.x
            },
            node.y + node.height / 2.0,
        );
        curve_commands_directed(start, from_right, (to_x, to_y), !from_right)
    }
}

fn contains(node: &NodeGeometry, x: f32, y: f32) -> bool {
    x >= node.x && x <= node.x + node.width && y >= node.y && y <= node.y + node.height
}

fn point_in_box(point: (f32, f32), x: f32, y: f32, width: f32, height: f32) -> bool {
    point.0 >= x && point.0 <= x + width && point.1 >= y && point.1 <= y + height
}

type Cubic = [(f32, f32); 4];

fn bezier(start: (f32, f32), end: (f32, f32)) -> Cubic {
    // Without direction information assume the usual output → input flow.
    directed_bezier(start, true, end, false)
}

fn directed_bezier(
    start: (f32, f32),
    start_outgoing: bool,
    end: (f32, f32),
    end_outgoing: bool,
) -> Cubic {
    let span = (end.0 - start.0).abs().max((end.1 - start.1).abs() * 0.5);
    let offset = (span * 0.5).clamp(45.0, 320.0);
    let start_sign = if start_outgoing { 1.0 } else { -1.0 };
    let end_sign = if end_outgoing { 1.0 } else { -1.0 };
    [
        start,
        (start.0 + start_sign * offset, start.1),
        (end.0 + end_sign * offset, end.1),
        end,
    ]
}

/// Squared distance from `point` to the nearest position on a line segment.
fn squared_distance_to_segment(point: (f32, f32), start: (f32, f32), end: (f32, f32)) -> f32 {
    let (vx, vy) = (end.0 - start.0, end.1 - start.1);
    let (wx, wy) = (point.0 - start.0, point.1 - start.1);
    let length_sq = vx * vx + vy * vy;
    if length_sq <= f32::EPSILON {
        return wx * wx + wy * wy;
    }
    let t = ((wx * vx + wy * vy) / length_sq).clamp(0.0, 1.0);
    let closest = (start.0 + t * vx, start.1 + t * vy);
    (point.0 - closest.0).powi(2) + (point.1 - closest.1).powi(2)
}

fn cubic_at(curve: &Cubic, t: f32) -> (f32, f32) {
    let u = 1.0 - t;
    let (a, b, c, d) = (u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t);
    (
        a * curve[0].0 + b * curve[1].0 + c * curve[2].0 + d * curve[3].0,
        a * curve[0].1 + b * curve[1].1 + c * curve[2].1 + d * curve[3].1,
    )
}

fn curve_commands(start: (f32, f32), end: (f32, f32)) -> String {
    commands_for(&bezier(start, end))
}

fn curve_commands_directed(
    start: (f32, f32),
    start_outgoing: bool,
    end: (f32, f32),
    end_outgoing: bool,
) -> String {
    commands_for(&directed_bezier(start, start_outgoing, end, end_outgoing))
}

fn commands_for(curve: &Cubic) -> String {
    format!(
        "M {:.2} {:.2} C {:.2} {:.2} {:.2} {:.2} {:.2} {:.2}",
        curve[0].0,
        curve[0].1,
        curve[1].0,
        curve[1].1,
        curve[2].0,
        curve[2].1,
        curve[3].0,
        curve[3].1
    )
}

/// SVG commands for the screen-space background grid.
pub(crate) fn grid_commands(
    width: f32,
    height: f32,
    zoom: f32,
    pan_x: f32,
    pan_y: f32,
    spacing: f32,
) -> String {
    let spacing = spacing * zoom;
    if spacing < 4.0 || width <= 0.0 || height <= 0.0 {
        return String::new();
    }
    let mut commands = String::with_capacity(2048);
    let mut x = pan_x.rem_euclid(spacing);
    while x < width {
        commands.push_str(&format!("M {x:.1} 0 L {x:.1} {height:.1} "));
        x += spacing;
    }
    let mut y = pan_y.rem_euclid(spacing);
    while y < height {
        commands.push_str(&format!("M 0 {y:.1} L {width:.1} {y:.1} "));
        y += spacing;
    }
    commands
}

/// Replace the selection with `id`, or toggle it when `extend` is set.
pub(crate) fn apply_click<T>(
    rows: &[T],
    id_of: impl Fn(&T) -> i32,
    selected_of: impl Fn(&T) -> bool,
    target: i32,
    extend: bool,
) -> Vec<bool> {
    rows.iter()
        .map(|row| {
            if id_of(row) == target {
                if extend {
                    !selected_of(row)
                } else {
                    true
                }
            } else if extend {
                selected_of(row)
            } else {
                false
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geometry() -> CanvasGeometry {
        let mut canvas = CanvasGeometry::default();
        canvas.replace(
            vec![
                NodeGeometry {
                    id: 7,
                    x: 100.0,
                    y: 100.0,
                    width: 244.0,
                    height: 120.0,
                    selected: false,
                    pins_visible: true,
                },
                NodeGeometry {
                    id: 8,
                    x: 500.0,
                    y: 100.0,
                    width: 244.0,
                    height: 120.0,
                    selected: false,
                    pins_visible: true,
                },
            ],
            vec![
                PinGeometry {
                    pin_id: 101,
                    node_id: 7,
                    is_output: true,
                    x: 100.0 + 244.0 - PIN_INSET,
                    y: 100.0 + port_row_top(0, false) + PORT_ROW_HEIGHT / 2.0,
                    visible: true,
                    node_selected: false,
                    connectable: true,
                },
                PinGeometry {
                    pin_id: 202,
                    node_id: 8,
                    is_output: false,
                    x: 500.0 + PIN_INSET,
                    y: 100.0 + port_row_top(0, false) + PORT_ROW_HEIGHT / 2.0,
                    visible: true,
                    node_selected: false,
                    connectable: true,
                },
            ],
            vec![LinkGeometry {
                id: 1,
                start_pin: 101,
                end_pin: 202,
            }],
            false,
        );
        canvas
    }

    #[test]
    fn pins_win_over_the_node_under_the_pointer() {
        let canvas = geometry();
        let pin = canvas.pin(101).unwrap();
        let hit = canvas.hit_test(pin.x, pin.y, 1.0);
        assert_eq!(hit.kind, HIT_PIN);
        assert_eq!(hit.id, 101);
        assert_eq!((hit.x, hit.y), (pin.x, pin.y));
    }

    #[test]
    fn pin_hit_test_tolerates_pointer_slack() {
        let canvas = geometry();
        let pin = canvas.pin(202).unwrap();
        assert_eq!(
            canvas.find_pin_at(pin.x + 8.0, pin.y + 4.0, PIN_SCREEN_HIT_RADIUS),
            202
        );
        assert_eq!(
            canvas.find_pin_at(pin.x + 40.0, pin.y, PIN_SCREEN_HIT_RADIUS),
            0
        );
    }

    #[test]
    fn node_press_reports_a_move_gesture_in_advanced_mode() {
        let canvas = geometry();
        let hit = canvas.hit_test(200.0, 110.0, 1.0);
        assert_eq!(hit.kind, HIT_NODE);
        assert_eq!(hit.id, 7);
        let body = canvas.hit_test(200.0, 190.0, 1.0);
        assert_eq!(body.kind, HIT_NODE);
    }

    #[test]
    fn easy_mode_turns_the_node_body_into_a_connection_gesture() {
        let mut canvas = geometry();
        canvas.easy_mode = true;
        assert_eq!(canvas.hit_test(200.0, 110.0, 1.0).kind, HIT_NODE);
        assert_eq!(canvas.hit_test(200.0, 190.0, 1.0).kind, HIT_NODE_BODY);
    }

    #[test]
    fn easy_mode_keeps_the_header_as_a_move_handle() {
        let mut canvas = geometry();
        canvas.easy_mode = true;
        // The header is a move surface at the very top and at its last pixel.
        assert_eq!(canvas.hit_test(200.0, 101.0, 1.0).kind, HIT_NODE);
        assert_eq!(
            canvas.hit_test(200.0, 100.0 + HEADER_HEIGHT, 1.0).kind,
            HIT_NODE
        );
        // Just below it the card becomes a connect surface.
        assert_eq!(
            canvas
                .hit_test(200.0, 100.0 + HEADER_HEIGHT + 1.0, 1.0)
                .kind,
            HIT_NODE_BODY
        );
    }

    #[test]
    fn easy_mode_connects_from_the_body_of_a_pinless_card() {
        let mut canvas = geometry();
        canvas.easy_mode = true;
        let mut nodes = canvas.nodes.clone();
        nodes[0].pins_visible = false;
        let mut pins: Vec<PinGeometry> = canvas.pins.values().copied().collect();
        pins[0].visible = false;
        let links = canvas.links.clone();
        canvas.replace(nodes, pins, links, true);

        // A collapsed card has no pin to drag from, so its body has to connect.
        assert_eq!(canvas.hit_test(200.0, 190.0, 1.0).kind, HIT_NODE_BODY);
        assert_eq!(canvas.hit_test(200.0, 110.0, 1.0).kind, HIT_NODE);
    }

    /// Dragging an edge pivots around the far end, so grabbing near the output
    /// anchors the input and the other way round.
    #[test]
    fn dragging_an_edge_anchors_its_far_end() {
        let canvas = geometry();
        let start = canvas.pin(101).unwrap();
        let end = canvas.pin(202).unwrap();

        assert_eq!(canvas.link_drag_anchor(1, start.x, start.y), 202);
        assert_eq!(canvas.link_drag_anchor(1, end.x, end.y), 101);
        // A grab just past the midpoint leans to the nearer endpoint.
        let midpoint = ((start.x + end.x) / 2.0, (start.y + end.y) / 2.0);
        assert_eq!(
            canvas.link_drag_anchor(1, midpoint.0 + 40.0, midpoint.1),
            101
        );
        assert_eq!(
            canvas.link_drag_anchor(1, midpoint.0 - 40.0, midpoint.1),
            202
        );
    }

    /// Backend-wide `connect` is a union across children, so on Windows it is
    /// true because MIDI can route while Core Audio cannot. Offering a connect
    /// gesture on a pin that can never accept one is the failure this prevents.
    #[test]
    fn a_pin_whose_backend_cannot_rewire_is_not_connectable() {
        let mut canvas = geometry();
        let mut pins: Vec<PinGeometry> = canvas.pins.values().copied().collect();
        pins[0].connectable = false;
        let nodes = canvas.nodes.clone();
        let links = canvas.links.clone();
        canvas.replace(nodes, pins, links, false);

        assert!(!canvas.pin_connectable(101));
        assert!(canvas.pin_connectable(202));
        // An unknown pin is never connectable.
        assert!(!canvas.pin_connectable(0));
        assert!(!canvas.pin_connectable(9999));
    }

    #[test]
    fn an_unknown_link_has_no_drag_anchor() {
        let canvas = geometry();
        assert_eq!(canvas.link_drag_anchor(999, 0.0, 0.0), 0);
    }

    #[test]
    fn links_are_hit_along_the_rendered_curve() {
        let canvas = geometry();
        let start = canvas.pin(101).unwrap();
        let end = canvas.pin(202).unwrap();
        let midpoint = ((start.x + end.x) / 2.0, (start.y + end.y) / 2.0);
        assert_eq!(
            canvas.find_link_at(midpoint.0, midpoint.1, LINK_SCREEN_HIT_RADIUS),
            1
        );
        assert_eq!(
            canvas.find_link_at(midpoint.0, midpoint.1 + 120.0, LINK_SCREEN_HIT_RADIUS),
            -1
        );
    }

    /// Two cards far enough apart that the drawn edge is long and curved, which
    /// is exactly where sampled-point hit testing used to leave gaps.
    fn long_link_geometry() -> CanvasGeometry {
        let mut canvas = CanvasGeometry::default();
        canvas.replace(
            vec![
                NodeGeometry {
                    id: 7,
                    x: 100.0,
                    y: 100.0,
                    width: 244.0,
                    height: 120.0,
                    selected: false,
                    pins_visible: true,
                },
                NodeGeometry {
                    id: 8,
                    x: 1500.0,
                    y: 700.0,
                    width: 244.0,
                    height: 120.0,
                    selected: false,
                    pins_visible: true,
                },
            ],
            vec![
                PinGeometry {
                    pin_id: 101,
                    node_id: 7,
                    is_output: true,
                    x: 100.0 + 244.0 - PIN_INSET,
                    y: 100.0 + port_row_top(0, false) + PORT_ROW_HEIGHT / 2.0,
                    visible: true,
                    node_selected: false,
                    connectable: true,
                },
                PinGeometry {
                    pin_id: 202,
                    node_id: 8,
                    is_output: false,
                    x: 1500.0 + PIN_INSET,
                    y: 700.0 + port_row_top(0, false) + PORT_ROW_HEIGHT / 2.0,
                    visible: true,
                    node_selected: false,
                    connectable: true,
                },
            ],
            vec![LinkGeometry {
                id: 1,
                start_pin: 101,
                end_pin: 202,
            }],
            false,
        );
        canvas
    }

    /// The point on the rendered curve at `t`, in world coordinates.
    fn point_on_link(canvas: &CanvasGeometry, link_id: i32, t: f32) -> (f32, f32) {
        let link = canvas
            .links
            .iter()
            .find(|link| link.id == link_id)
            .expect("link is cached");
        let start = canvas.pin(link.start_pin).expect("start pin is cached");
        let end = canvas.pin(link.end_pin).expect("end pin is cached");
        let curve = bezier(
            canvas.anchor(&start, (0.0, 0.0)),
            canvas.anchor(&end, (0.0, 0.0)),
        );
        cubic_at(&curve, t)
    }

    #[test]
    fn link_hit_test_detects_points_between_bezier_samples() {
        let canvas = long_link_geometry();
        // 0.37 lands between the old sample stops; so does every other step
        // here, which is the whole point: the curve is continuous, not dotted.
        let awkward = point_on_link(&canvas, 1, 0.37);
        assert_eq!(
            canvas.find_link_at(awkward.0, awkward.1, LINK_SCREEN_HIT_RADIUS),
            1
        );

        for step in 0..=200 {
            let t = step as f32 / 200.0;
            let point = point_on_link(&canvas, 1, t);
            assert_eq!(
                canvas.find_link_at(point.0, point.1, LINK_SCREEN_HIT_RADIUS),
                1,
                "the pointer sits on the drawn curve at t={t}"
            );
        }
    }

    #[test]
    fn link_hit_test_rejects_points_far_from_curve() {
        let canvas = long_link_geometry();
        let point = point_on_link(&canvas, 1, 0.37);
        assert_eq!(
            canvas.find_link_at(
                point.0,
                point.1 + LINK_SCREEN_HIT_RADIUS * 6.0,
                LINK_SCREEN_HIT_RADIUS
            ),
            -1
        );
        assert_eq!(canvas.find_link_at(0.0, 0.0, LINK_SCREEN_HIT_RADIUS), -1);
    }

    #[test]
    fn hit_tolerance_is_measured_in_screen_pixels_not_world_units() {
        let canvas = long_link_geometry();
        let point = point_on_link(&canvas, 1, 0.37);
        // 16 world units off the curve: further than the 9px tolerance at 1:1,
        // but well inside it once the canvas is halved, because the pointer is
        // then only 8 screen pixels away from the drawn line.
        let near_miss = (point.0, point.1 + 16.0);

        assert_eq!(
            canvas.hit_test(near_miss.0, near_miss.1, 1.0).kind,
            HIT_NONE
        );
        let zoomed = canvas.hit_test(near_miss.0, near_miss.1, 0.5);
        assert_eq!(zoomed.kind, HIT_LINK);
        assert_eq!(zoomed.id, 1);

        // Zooming in tightens the world tolerance by the same rule.
        let pin = canvas.pin(202).unwrap();
        assert_eq!(
            canvas.hit_test(pin.x, pin.y + 8.0, 1.0).kind,
            HIT_PIN,
            "8 world units is inside the 11px pin tolerance at 1:1"
        );
        assert_ne!(
            canvas.hit_test(pin.x - 8.0, pin.y - 8.0, 2.5).kind,
            HIT_PIN,
            "the same offset is far outside the pin once magnified"
        );
    }

    #[test]
    fn hit_test_survives_a_degenerate_zoom() {
        let canvas = long_link_geometry();
        let point = point_on_link(&canvas, 1, 0.37);
        for zoom in [0.0, -1.0, f32::NAN, f32::INFINITY] {
            let hit = canvas.hit_test(point.0, point.1, zoom);
            assert_eq!(hit.kind, HIT_LINK, "zoom {zoom} falls back to 1:1");
        }
    }

    #[test]
    fn pin_wins_over_link_at_endpoint() {
        let canvas = long_link_geometry();
        let pin = canvas.pin(202).unwrap();
        let hit = canvas.hit_test(pin.x, pin.y, 1.0);
        assert_eq!(hit.kind, HIT_PIN);
        assert_eq!(hit.id, 202);
    }

    #[test]
    fn node_wins_when_a_link_passes_behind_a_card() {
        let mut canvas = long_link_geometry();
        // Park a third card straight over the middle of the curve.
        let middle = point_on_link(&canvas, 1, 0.5);
        let mut nodes = canvas.nodes.clone();
        nodes.push(NodeGeometry {
            id: 9,
            x: middle.0 - 122.0,
            y: middle.1 - 60.0,
            width: 244.0,
            height: 120.0,
            selected: false,
            pins_visible: true,
        });
        let pins: Vec<PinGeometry> = canvas.pins.values().copied().collect();
        let links = canvas.links.clone();
        canvas.replace(nodes, pins, links, false);

        let hit = canvas.hit_test(middle.0, middle.1, 1.0);
        assert_eq!(hit.kind, HIT_NODE, "cards stay clickable through an edge");
        assert_eq!(hit.id, 9);
    }

    #[test]
    fn link_paths_follow_the_live_drag_offset() {
        let mut canvas = geometry();
        let resting = canvas.link_path(1, (0.0, 0.0));
        assert!(resting.starts_with("M 334.00"));

        let mut pins: Vec<PinGeometry> = canvas.pins.values().copied().collect();
        pins[0].node_selected = true;
        let nodes = canvas.nodes.clone();
        let links = canvas.links.clone();
        canvas.replace(nodes, pins, links, false);
        let dragged = canvas.link_path(1, (30.0, 20.0));
        assert!(dragged.starts_with("M 364.00"));
    }

    #[test]
    fn box_selection_reports_nodes_and_links_inside_it() {
        let canvas = geometry();
        assert_eq!(canvas.nodes_in_box(90.0, 90.0, 300.0, 200.0), vec![7]);
        assert_eq!(canvas.links_in_box(90.0, 90.0, 300.0, 200.0), vec![1]);
        assert!(canvas.nodes_in_box(0.0, 0.0, 10.0, 10.0).is_empty());
        assert!(canvas.links_in_box(0.0, 0.0, 10.0, 10.0).is_empty());
    }

    #[test]
    fn collapsed_cards_anchor_their_links_to_the_card_edge() {
        let mut canvas = geometry();
        let mut nodes = canvas.nodes.clone();
        nodes[0].pins_visible = false;
        let mut pins: Vec<PinGeometry> = canvas.pins.values().copied().collect();
        pins[0].visible = false;
        let links = canvas.links.clone();
        canvas.replace(nodes, pins, links, false);
        assert!(canvas
            .link_path(1, (0.0, 0.0))
            .starts_with("M 344.00 120.00"));
        assert_eq!(canvas.find_pin_at(344.0, 120.0, PIN_SCREEN_HIT_RADIUS), 0);
    }

    #[test]
    fn grid_lines_follow_pan_and_zoom() {
        let commands = grid_commands(100.0, 100.0, 1.0, 0.0, 0.0, 24.0);
        assert!(commands.contains("M 0.0 0 L 0.0 100.0"));
        assert!(commands.contains("M 24.0 0 L 24.0 100.0"));
        assert!(grid_commands(100.0, 100.0, 0.1, 0.0, 0.0, 24.0).is_empty());
    }

    #[test]
    fn clicking_replaces_the_selection_unless_extended() {
        let rows = vec![(1, false), (2, true)];
        let replaced = apply_click(&rows, |row| row.0, |row| row.1, 1, false);
        assert_eq!(replaced, vec![true, false]);
        let extended = apply_click(&rows, |row| row.0, |row| row.1, 1, true);
        assert_eq!(extended, vec![true, true]);
        let toggled = apply_click(&rows, |row| row.0, |row| row.1, 2, true);
        assert_eq!(toggled, vec![false, false]);
    }
}
