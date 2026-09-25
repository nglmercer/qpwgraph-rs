//! Persistent connection sets and activation policies.
//!
//! The native qpwgraph format is XML and resolves rules by node/port names.
//! JSON remains supported as a convenient machine-readable format for tooling
//! and for compatibility with the first Rust prototype.

use super::model::{PatchConnection, Patchbay, XmlSelectorSidecarEntry};
use pw_graph_core::{
    Direction, EndpointResolution, EndpointSelector, Graph, NodeIdentity, NodeType, PortType,
};
use std::path::{Path, PathBuf};

pub(crate) fn legacy_selector(
    node_name: String,
    port_name: String,
    direction: Direction,
    port_type: PortType,
    node_type: NodeType,
) -> EndpointSelector {
    EndpointSelector {
        node_type,
        identity: NodeIdentity::with_node_name(node_name),
        port_name,
        channel: None,
        direction,
        port_type,
        match_mode: pw_graph_core::EndpointMatchMode::Instance,
    }
}

pub(crate) fn connection_selectors(
    connection: &PatchConnection,
) -> (EndpointSelector, EndpointSelector) {
    let output = connection.output_selector.clone().unwrap_or_else(|| {
        legacy_selector(
            connection.output_node.clone(),
            connection.output_name.clone(),
            Direction::Source,
            connection.port_type,
            connection.effective_output_node_type(),
        )
    });
    let input = connection.input_selector.clone().unwrap_or_else(|| {
        legacy_selector(
            connection.input_node.clone(),
            connection.input_name.clone(),
            Direction::Sink,
            connection.port_type,
            connection.effective_input_node_type(),
        )
    });
    (output, input)
}

pub(crate) fn resolve_selector<F>(
    graph: &Graph,
    selector: Option<&EndpointSelector>,
    legacy: F,
) -> EndpointResolution
where
    F: FnOnce() -> EndpointSelector,
{
    let selector = selector.cloned().unwrap_or_else(legacy);
    let result = graph.resolve_endpoint(&selector);
    if !matches!(result, EndpointResolution::Missing) {
        return result;
    }
    // A pre-v2 rule used one broad node type for both endpoints. Preserve the
    // old compatibility fallback, but only after the precise typed selector
    // had no candidate.
    if selector.node_type != NodeType::Unknown && selector.identity.effect_instance_id.is_none() {
        let mut fallback = selector;
        fallback.node_type = NodeType::Unknown;
        return graph.resolve_endpoint(&fallback);
    }
    result
}

pub(crate) fn selectors_equivalent(saved: &EndpointSelector, current: &EndpointSelector) -> bool {
    let app_channel_matches = !matches!(
        saved.match_mode,
        pw_graph_core::EndpointMatchMode::NamePattern
    ) && !matches!(
        current.match_mode,
        pw_graph_core::EndpointMatchMode::NamePattern
    ) && saved.channel.is_some()
        && saved.channel == current.channel
        && (saved.identity.application_id.is_some()
            || saved.identity.process_binary.is_some()
            || saved.identity.application_name.is_some());
    let port_name_matches = saved.port_name == current.port_name || app_channel_matches;
    if saved.node_type != current.node_type
        || !port_name_matches
        || saved.channel != current.channel
        || saved.direction != current.direction
        || saved.port_type != current.port_type
    {
        return false;
    }
    let a = &saved.identity;
    let b = &current.identity;
    if a.effect_instance_id.is_some() || b.effect_instance_id.is_some() {
        return a.effect_instance_id == b.effect_instance_id;
    }
    if a.application_id.is_some() || b.application_id.is_some() {
        return a.application_id.is_some() && a.application_id == b.application_id;
    }
    ((a.process_binary.is_some() || b.process_binary.is_some())
        && a.process_binary == b.process_binary
        && (a.application_name.is_none()
            || b.application_name.is_none()
            || a.application_name == b.application_name))
        || (a.application_id.is_none()
            && b.application_id.is_none()
            && a.process_binary.is_none()
            && b.process_binary.is_none()
            && a.node_name == b.node_name)
}

pub(crate) fn selector_sidecar_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("patchbay");
    path.parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!("{file_name}.qpwgraph-rs-selectors.json"))
}

pub(crate) fn connection_fingerprint(connection: &PatchConnection) -> String {
    serde_json::to_string(&(
        &connection.output_node,
        &connection.output_name,
        &connection.input_node,
        &connection.input_name,
        connection.node_type,
        connection.output_node_type,
        connection.input_node_type,
        connection.port_type,
    ))
    .expect("patchbay selector fingerprint is serializable")
}

pub(crate) fn load_xml_selector_sidecar(path: &Path, patchbay: &mut Patchbay) {
    let sidecar = selector_sidecar_path(path);
    let Ok(text) = std::fs::read_to_string(sidecar) else {
        return;
    };
    let Ok(entries) = serde_json::from_str::<Vec<XmlSelectorSidecarEntry>>(&text) else {
        return;
    };
    let mut used = vec![false; entries.len()];
    for (connection_index, connection) in patchbay.connections.iter_mut().enumerate() {
        let fingerprint = connection_fingerprint(connection);
        let match_index = entries
            .iter()
            .enumerate()
            .find(|(index, entry)| !used[*index] && entry.index == Some(connection_index))
            .map(|(index, _)| index)
            .or_else(|| {
                entries
                    .iter()
                    .enumerate()
                    .find(|(index, entry)| !used[*index] && entry.fingerprint == fingerprint)
                    .map(|(index, _)| index)
            });
        let Some(index) = match_index else {
            continue;
        };
        let entry = &entries[index];
        connection.output_selector = entry.output_selector.clone();
        connection.input_selector = entry.input_selector.clone();
        connection.output_node_type = entry.output_node_type;
        connection.input_node_type = entry.input_node_type;
        if let Some(port_type) = entry.port_type {
            connection.port_type = port_type;
        }
        used[index] = true;
    }
    patchbay.version = patchbay.version.max(2);
}
