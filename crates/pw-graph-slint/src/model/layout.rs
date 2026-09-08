//! Moving nodes: where a drag may land, and how saved positions are
//! restored onto a freshly discovered graph.

use super::*;

pub(super) const COLLISION_GAP: f32 = 18.0;

/// Resolve a user-requested drag against the exact visible card rectangles.
///
/// Every selected node receives the same returned delta, preserving the group
/// as a rigid object. Only obstacles the requested drop actually overlaps
/// seed candidates -- one push per colliding edge -- so the search stays
/// proportional to nearby collisions instead of the Cartesian product of
/// every card edge. Candidates are ranked by distance from the requested
/// drop, with a stable coordinate tie-breaker.
pub(crate) fn resolve_drag_delta(
    snapshot: &GraphSnapshot,
    selected: &BTreeSet<NodeId>,
    desired: [f32; 2],
    repel: bool,
) -> [f32; 2] {
    if !repel || selected.is_empty() {
        return desired;
    }

    let dragged = snapshot
        .nodes
        .iter()
        .filter(|node| selected.contains(&node.node_id))
        .collect::<Vec<_>>();
    let stationary = snapshot
        .nodes
        .iter()
        .filter(|node| !selected.contains(&node.node_id))
        .collect::<Vec<_>>();
    if dragged.is_empty() || stationary.is_empty() || drag_is_clear(&dragged, &stationary, desired)
    {
        return desired;
    }

    // Bounded local search: each round pushes out of the obstacles the probe
    // still overlaps. One round suffices for the common single-collision
    // drop; the bound keeps wedged groups from iterating forever.
    let mut probe = desired;
    for _ in 0..8 {
        let colliding = colliding_obstacles(&dragged, &stationary, probe);
        if colliding.is_empty() {
            return probe;
        }
        let mut candidates = Vec::with_capacity(colliding.len() * 4 + 1);
        for (moving, obstacle) in &colliding {
            let left = obstacle.position[0] - COLLISION_GAP - moving.width - moving.position[0];
            let right = obstacle.position[0] + obstacle.width + COLLISION_GAP - moving.position[0];
            let above = obstacle.position[1] - COLLISION_GAP - moving.height - moving.position[1];
            let below = obstacle.position[1] + obstacle.height + COLLISION_GAP - moving.position[1];
            candidates.push([left, probe[1]]);
            candidates.push([right, probe[1]]);
            candidates.push([probe[0], above]);
            candidates.push([probe[0], below]);
        }
        candidates.sort_by(|left, right| {
            left[0]
                .total_cmp(&right[0])
                .then_with(|| left[1].total_cmp(&right[1]))
        });
        candidates.dedup_by(|left, right| {
            left[0].total_cmp(&right[0]).is_eq() && left[1].total_cmp(&right[1]).is_eq()
        });
        if let Some(best) = candidates
            .iter()
            .copied()
            .filter(|candidate| drag_is_clear(&dragged, &stationary, *candidate))
            .min_by(|left, right| {
                drag_distance_squared(*left, desired)
                    .total_cmp(&drag_distance_squared(*right, desired))
                    .then_with(|| left[1].total_cmp(&right[1]))
                    .then_with(|| left[0].total_cmp(&right[0]))
            })
        {
            return best;
        }
        // No single-edge push clears the group; step toward the candidate
        // with the fewest remaining overlaps so the next round can finish.
        let Some(next) = candidates.iter().copied().min_by(|left, right| {
            collision_count(&dragged, &stationary, *left)
                .cmp(&collision_count(&dragged, &stationary, *right))
                .then_with(|| {
                    drag_distance_squared(*left, desired)
                        .total_cmp(&drag_distance_squared(*right, desired))
                })
                .then_with(|| left[1].total_cmp(&right[1]))
                .then_with(|| left[0].total_cmp(&right[0]))
        }) else {
            return desired;
        };
        if next == probe {
            return desired;
        }
        probe = next;
    }
    if drag_is_clear(&dragged, &stationary, probe) {
        probe
    } else {
        desired
    }
}

/// The (moving, obstacle) pairs that overlap at `delta`.
fn colliding_obstacles<'a>(
    dragged: &[&'a NodeView],
    stationary: &[&'a NodeView],
    delta: [f32; 2],
) -> Vec<(&'a NodeView, &'a NodeView)> {
    let mut pairs = Vec::new();
    for moving in dragged {
        let position = [moving.position[0] + delta[0], moving.position[1] + delta[1]];
        for obstacle in stationary {
            if intersects(
                position,
                [moving.width, moving.height],
                obstacle.position[0] - COLLISION_GAP,
                obstacle.position[1] - COLLISION_GAP,
                obstacle.width + COLLISION_GAP * 2.0,
                obstacle.height + COLLISION_GAP * 2.0,
            ) {
                pairs.push((*moving, *obstacle));
            }
        }
    }
    pairs
}

fn collision_count(dragged: &[&NodeView], stationary: &[&NodeView], delta: [f32; 2]) -> usize {
    colliding_obstacles(dragged, stationary, delta).len()
}

pub(super) fn drag_is_clear(
    dragged: &[&NodeView],
    stationary: &[&NodeView],
    delta: [f32; 2],
) -> bool {
    dragged.iter().all(|moving| {
        let position = [moving.position[0] + delta[0], moving.position[1] + delta[1]];
        stationary.iter().all(|obstacle| {
            !intersects(
                position,
                [moving.width, moving.height],
                obstacle.position[0] - COLLISION_GAP,
                obstacle.position[1] - COLLISION_GAP,
                obstacle.width + COLLISION_GAP * 2.0,
                obstacle.height + COLLISION_GAP * 2.0,
            )
        })
    })
}

pub(super) fn drag_distance_squared(candidate: [f32; 2], desired: [f32; 2]) -> f32 {
    let dx = candidate[0] - desired[0];
    let dy = candidate[1] - desired[1];
    dx * dx + dy * dy
}

pub(crate) fn node_layout_key(node: &Node) -> String {
    let kind = match node.node_type {
        NodeType::PipeWire => "PipeWire",
        NodeType::Effect => "Effect",
        NodeType::AlsaMidi => "AlsaMidi",
        NodeType::WindowsAudioEndpoint => "WindowsAudioEndpoint",
        NodeType::WindowsAudioSession => "WindowsAudioSession",
        NodeType::WindowsMidi => "WindowsMidi",
        NodeType::Unknown => "Unknown",
    };
    format!("{kind}:{}", node.name)
}

/// Apply the same stable layout lookup used by the rendered projection to the
/// backend, preserving startup position restoration semantics.
pub(crate) fn restore_node_positions(driver: &mut dyn GraphDriver, config: &AppConfig) {
    let positions = configured_positions(driver.graph(), config);
    for (node, position) in positions {
        let _ = driver.set_node_position(node, position);
    }
}
