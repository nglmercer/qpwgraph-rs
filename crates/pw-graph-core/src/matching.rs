//! Core graph types shared by every backend and presentation layer.

use super::endpoint::{EndpointMatchMode, EndpointSelector, NodeType};
use super::node::Node;

pub(crate) fn endpoint_match_score(node: &Node, selector: &EndpointSelector) -> Option<u16> {
    let actual = node.matching_identity();
    let expected = &selector.identity;

    if let Some(effect_instance_id) = expected.effect_instance_id.as_ref() {
        return (actual.effect_instance_id.as_ref() == Some(effect_instance_id)).then_some(1_000);
    }

    if let Some(application_id) = expected.application_id.as_ref() {
        if actual.application_id.as_ref() != Some(application_id) {
            return None;
        }
        let mut score = 800;
        if matches!(selector.match_mode, EndpointMatchMode::Instance) {
            if !expected.node_name.is_empty() && actual.node_name == expected.node_name {
                score += 70;
            }
            if expected.media_role.is_some() && expected.media_role == actual.media_role {
                score += 20;
            }
            // object.serial is only a same-session refinement after a
            // durable application identity matched; it is never sufficient
            // on its own.
            if expected.object_serial.is_some() && expected.object_serial == actual.object_serial {
                score += 10;
            }
        }
        return Some(score);
    }

    let has_process_fallback =
        expected.process_binary.is_some() || expected.application_name.is_some();
    if has_process_fallback {
        if expected.process_binary.is_some() && expected.process_binary != actual.process_binary {
            return None;
        }
        if expected.application_name.is_some()
            && expected.application_name != actual.application_name
        {
            return None;
        }
        let mut score = 620;
        if expected.media_role.is_some() && expected.media_role == actual.media_role {
            score += 30;
        }
        if matches!(selector.match_mode, EndpointMatchMode::Instance)
            && !expected.node_name.is_empty()
            && actual.node_name == expected.node_name
        {
            score += 30;
        }
        return Some(score);
    }

    // A PipeWire object.serial is intentionally not accepted as a
    // cross-restart application identity. It remains a useful current-session
    // hint for legacy Windows/native endpoint keys, and only after the node
    // type/name still agrees for PipeWire.
    if expected.object_serial.is_some()
        && expected.object_serial == actual.object_serial
        && (selector.node_type != NodeType::PipeWire
            || expected.node_name.is_empty()
            || expected.node_name == actual.node_name
            || expected.node_name == node.name)
    {
        return Some(600);
    }

    if expected.node_name.is_empty() {
        return Some(100);
    }
    let name_matches = match selector.match_mode {
        EndpointMatchMode::NamePattern => {
            let pattern = expected
                .node_name
                .strip_suffix('*')
                .unwrap_or(&expected.node_name);
            node.name.starts_with(pattern) || actual.node_name.starts_with(pattern)
        }
        EndpointMatchMode::Instance | EndpointMatchMode::Application => {
            node.name == expected.node_name || actual.node_name == expected.node_name
        }
    };
    name_matches.then_some(450)
}

pub(crate) fn endpoint_identity_basis(selector: &EndpointSelector) -> &'static str {
    let identity = &selector.identity;
    if identity.effect_instance_id.is_some() {
        "effect instance id"
    } else if identity.application_id.is_some() {
        "application.id"
    } else if identity.process_binary.is_some() || identity.application_name.is_some() {
        "process binary/application name"
    } else if identity.object_serial.is_some() {
        "object.serial session hint"
    } else if matches!(selector.match_mode, EndpointMatchMode::NamePattern) {
        "name pattern"
    } else if identity.node_name.is_empty() {
        "unqualified selector"
    } else {
        "legacy node name + port name"
    }
}
