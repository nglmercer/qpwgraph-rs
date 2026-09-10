use crate::model::ConnectMode;
use pw_graph_command::{
    ConnectCommand, ConnectManyCommand, DisconnectManyCommand, RerouteLinkCommand,
};
use pw_graph_core::{Direction, PortId, PortKey};
use std::time::Instant;

use super::app::{set_connection_feedback, Application};

/// Move one end of an existing edge onto a different pin.
///
/// This is one user action, so it is one undoable command rather than a
/// disconnect followed by a connect: undoing once has to put the edge back
/// where it was, not leave the graph disconnected.
pub(crate) fn handle_link_rerouted(application: &mut Application, link_id: i32, pin_id: i32) {
    let capabilities = application.source.capabilities();
    if !capabilities.connect || !capabilities.disconnect {
        set_connection_feedback(
            application,
            application.t("status.connections_unavailable"),
            true,
        );
        return;
    }
    let (Some(link), Some(port)) = (
        application.view.ids.link_id(link_id),
        application.view.ids.port_id(pin_id),
    ) else {
        return;
    };
    if !application.source.is_link_mutable(link) {
        // An observed relationship is not ours to move.
        set_connection_feedback(
            application,
            application.t("status.connections_unavailable"),
            true,
        );
        return;
    }
    let target_connectable = application
        .source
        .graph()
        .port(port)
        .is_some_and(|port| application.source.node_connectable(port.node_id));
    if !target_connectable {
        set_connection_feedback(
            application,
            application.t("status.connections_unavailable"),
            true,
        );
        return;
    }
    match application.commands.execute(
        Box::new(RerouteLinkCommand::new(link, port)),
        &mut application.source,
    ) {
        Ok(()) => {
            let message = application.t("status.connection_created");
            set_connection_feedback(application, message, false);
        }
        Err(error) => {
            let message =
                application.tf("status.connection_failed", &[("error", error.to_string())]);
            set_connection_feedback(application, message, true);
        }
    }
}

pub(crate) fn handle_link_requested(application: &mut Application, start_id: i32, end_id: i32) {
    if !application.source.capabilities().connect {
        set_connection_feedback(
            application,
            application.t("status.connections_unavailable"),
            true,
        );
        return;
    }
    if start_id != end_id {
        application.pending_connection_pin = None;
        if application.view.connect_mode == ConnectMode::Easy {
            // A rendered pin stands for a whole channel group here, so the
            // drag connects every channel it holds, not just the first one.
            easy_connect_pin_pair(application, start_id, end_id);
        } else {
            connect_pin_pair(application, start_id, end_id);
        }
        return;
    }

    match application.pending_connection_pin.take() {
        None => {
            application.pending_connection_pin = Some(start_id);
            let message = application.t("status.connection_started");
            set_connection_feedback(application, message, false);
        }
        Some(pending) if pending == start_id => {
            let message = application.t("status.connection_cancelled");
            set_connection_feedback(application, message, false);
        }
        Some(pending) if application.view.connect_mode == ConnectMode::Easy => {
            easy_connect_pin_pair(application, pending, start_id);
        }
        Some(pending) => connect_pin_pair(application, pending, start_id),
    }
}

pub(crate) fn connect_pin_pair(application: &mut Application, start_id: i32, end_id: i32) {
    if !application.source.capabilities().connect {
        set_connection_feedback(
            application,
            application.t("status.connections_unavailable"),
            true,
        );
        return;
    }
    if start_id == end_id {
        let message = application.t("status.connection_cancelled");
        set_connection_feedback(application, message, false);
        return;
    }

    let Some(start_port) = application.view.ids.port_id(start_id) else {
        let message = application.tf(
            "status.connection_pin_missing",
            &[("pin", start_id.to_string())],
        );
        set_connection_feedback(application, message, true);
        return;
    };
    let Some(end_port) = application.view.ids.port_id(end_id) else {
        let message = application.tf(
            "status.connection_pin_missing",
            &[("pin", end_id.to_string())],
        );
        set_connection_feedback(application, message, true);
        return;
    };

    let (output_port, input_port) = {
        let graph = application.source.graph();
        let Some(start) = graph.port(start_port) else {
            let message = application.t("status.connection_pin_missing");
            set_connection_feedback(application, message, true);
            return;
        };
        let Some(end) = graph.port(end_port) else {
            let message = application.t("status.connection_pin_missing");
            set_connection_feedback(application, message, true);
            return;
        };
        match (start.direction, end.direction) {
            (Direction::Source, Direction::Sink) => (start_port, end_port),
            (Direction::Sink, Direction::Source) => (end_port, start_port),
            _ => {
                let message = application.t("status.connection_direction");
                set_connection_feedback(application, message, true);
                return;
            }
        }
    };

    if !pair_is_connectable(application, output_port, input_port) {
        let message = application.t("status.connections_unavailable");
        set_connection_feedback(application, message, true);
        return;
    }

    let Some((output, input)) = ({
        let graph = application.source.graph();
        graph.port_key(output_port).zip(graph.port_key(input_port))
    }) else {
        let message = application.t("status.connection_identity");
        set_connection_feedback(application, message, true);
        return;
    };

    let existed = application
        .source
        .graph()
        .find_link_by_keys(&output, &input)
        .is_some();
    match application.commands.execute(
        Box::new(ConnectCommand::from_keys(output.clone(), input.clone())),
        &mut application.source,
    ) {
        Ok(()) => match refresh_connection_graph(application) {
            Ok(()) => {
                application.allow_patchbay_connections(std::slice::from_ref(&(
                    output.clone(),
                    input.clone(),
                )));
                application.sync_patchbay_connections();
                application.autosave_patchbay();
                let message = if !existed {
                    application.t("status.connection_created")
                } else {
                    application.t("status.connection_already_exists")
                };
                set_connection_feedback(application, message, false);
            }
            Err(error) => set_connection_feedback(
                application,
                application.tf(
                    "status.connection_succeeded_refresh_failed",
                    &[("error", error)],
                ),
                true,
            ),
        },
        Err(error) => {
            let message =
                application.tf("status.connection_failed", &[("error", error.to_string())]);
            set_connection_feedback(application, message, true);
        }
    }
}

pub(crate) fn refresh_connection_graph(application: &mut Application) -> Result<(), String> {
    application.source.refresh()?;
    application.last_refresh = Instant::now();
    Ok(())
}

pub(crate) fn easy_connect_nodes(
    application: &mut Application,
    source_id: i32,
    x: f32,
    y: f32,
    target_pin_id: i32,
) {
    if !application.source.capabilities().connect {
        set_connection_feedback(
            application,
            application.t("status.connections_unavailable"),
            true,
        );
        return;
    }
    let Some(source_node) = application.view.ids.node_id(source_id) else {
        let message = application.t("status.connection_pin_missing");
        set_connection_feedback(application, message, true);
        return;
    };
    // Prefer the actual rendered pin under the release. This is authoritative
    // even when a transformed/captured TouchArea reports imperfect card-local
    // coordinates at the edge of another node.
    let target_from_pin = application
        .view
        .ids
        .port_id(target_pin_id)
        .and_then(|port| application.source.graph().port(port))
        .map(|port| port.node_id)
        .filter(|node| *node != source_node);
    if target_from_pin.is_some() {
        // The drop landed on a rendered pin, so fill just that group instead
        // of everything the destination card exposes.
        let port_keys = {
            let graph = application.source.graph();
            application
                .view
                .matching_group_to_node_pairs(graph, target_pin_id, source_node)
                .into_iter()
                .filter_map(|(output, input)| {
                    Some((graph.port_key(output)?, graph.port_key(input)?))
                })
                .collect::<Vec<_>>()
        };
        if !port_keys.is_empty() {
            apply_easy_pairs(application, port_keys);
            return;
        }
    }
    let Some(target_node) = target_from_pin.or_else(|| {
        // A card-body drop has no pin identity. Keep coordinate hit-testing as
        // its fallback, including the small margin occupied by edge pins.
        application
            .view
            .node_at(&application.snapshot, x, y, source_node)
            .or_else(|| {
                application
                    .view
                    .node_at_with_margin(&application.snapshot, x, y, source_node, 12.0)
            })
    }) else {
        let message = application.t("status.easy_connect_cancelled");
        set_connection_feedback(application, message, true);
        return;
    };
    easy_connect_node_pair(application, source_node, target_node);
}

pub(crate) fn easy_connect_from_pin(
    application: &mut Application,
    source_pin_id: i32,
    x: f32,
    y: f32,
) {
    if !application.source.capabilities().connect {
        set_connection_feedback(
            application,
            application.t("status.connections_unavailable"),
            true,
        );
        return;
    }
    let Some(source_node) = application
        .view
        .ids
        .port_id(source_pin_id)
        .and_then(|port| application.source.graph().port(port))
        .map(|port| port.node_id)
    else {
        let message = application.t("status.connection_pin_missing");
        set_connection_feedback(application, message, true);
        return;
    };
    let Some(source_id) = application.view.ids.node(source_node) else {
        let message = application.t("status.connection_pin_missing");
        set_connection_feedback(application, message, true);
        return;
    };
    // The drag began on a pin, so connect that group's channels rather than
    // everything the destination card happens to expose. Whole-card pairing
    // stays as the fallback when the group has nothing to face.
    let target_node = application
        .view
        .node_at(&application.snapshot, x, y, source_node)
        .or_else(|| {
            application
                .view
                .node_at_with_margin(&application.snapshot, x, y, source_node, 12.0)
        });
    if let Some(target_node) = target_node {
        let port_keys = {
            let graph = application.source.graph();
            application
                .view
                .matching_group_to_node_pairs(graph, source_pin_id, target_node)
                .into_iter()
                .filter_map(|(output, input)| {
                    Some((graph.port_key(output)?, graph.port_key(input)?))
                })
                .collect::<Vec<_>>()
        };
        if !port_keys.is_empty() {
            application.pending_connection_pin = None;
            apply_easy_pairs(application, port_keys);
            return;
        }
    }
    easy_connect_nodes(application, source_id, x, y, 0);
}

fn easy_connect_pin_pair(application: &mut Application, source_pin_id: i32, target_pin_id: i32) {
    let nodes = {
        let graph = application.source.graph();
        application
            .view
            .ids
            .port_id(source_pin_id)
            .and_then(|source| graph.port(source))
            .map(|port| port.node_id)
            .zip(
                application
                    .view
                    .ids
                    .port_id(target_pin_id)
                    .and_then(|target| graph.port(target))
                    .map(|port| port.node_id),
            )
    };
    let Some((source_node, target_node)) = nodes else {
        let message = application.t("status.connection_pin_missing");
        set_connection_feedback(application, message, true);
        return;
    };
    if source_node == target_node {
        let message = application.t("status.connection_cancelled");
        set_connection_feedback(application, message, false);
        return;
    }

    // Pair the channels inside the two groups the user actually aimed at.
    let port_keys = {
        let graph = application.source.graph();
        application
            .view
            .matching_pin_pairs(graph, source_pin_id, target_pin_id)
            .into_iter()
            .filter_map(|(output, input)| Some((graph.port_key(output)?, graph.port_key(input)?)))
            .collect::<Vec<_>>()
    };
    if port_keys.is_empty() {
        let message = application.t("status.easy_connect_invalid");
        set_connection_feedback(application, message, true);
        return;
    }
    apply_easy_pairs(application, port_keys);
}

fn easy_connect_node_pair(
    application: &mut Application,
    source_node: pw_graph_core::NodeId,
    target_node: pw_graph_core::NodeId,
) {
    let port_keys = {
        let graph = application.source.graph();
        let mut pairs = application
            .view
            .matching_port_pairs(graph, source_node, target_node)
            .into_iter()
            .filter_map(|(output, input)| Some((graph.port_key(output)?, graph.port_key(input)?)))
            .collect::<Vec<_>>();
        // In precise mode this card gesture means connect-through: resolve
        // only the first compatible pair. Easy mode keeps its all-channel
        // semantics.
        if application.config.connect_through_nodes
            && application.view.connect_mode == ConnectMode::Advanced
        {
            pairs.truncate(1);
        }
        pairs
    };
    if port_keys.is_empty() {
        let message = application.t("status.easy_connect_no_pairs");
        set_connection_feedback(application, message, true);
        return;
    }
    apply_easy_pairs(application, port_keys);
}

/// Create every pair an Easy-mode gesture resolved to, then report what
/// happened as a single message.
fn apply_easy_pairs(application: &mut Application, port_keys: Vec<(PortKey, PortKey)>) {
    let port_keys: Vec<_> = port_keys
        .into_iter()
        .filter(|(output, input)| {
            let Some((output_id, input_id)) = application
                .source
                .graph()
                .resolve_port_key(output)
                .zip(application.source.graph().resolve_port_key(input))
            else {
                return false;
            };
            pair_is_connectable(application, output_id, input_id)
        })
        .collect();
    if port_keys.is_empty() {
        let message = application.t("status.connections_unavailable");
        set_connection_feedback(application, message, true);
        return;
    }
    let already_connected = port_keys
        .iter()
        .filter(|(output, input)| {
            application
                .source
                .graph()
                .find_link_by_keys(output, input)
                .is_some()
        })
        .count();
    let requested = port_keys.len();
    let result = application.commands.execute(
        Box::new(ConnectManyCommand::with_keys(Vec::new(), port_keys.clone())),
        &mut application.source,
    );
    let connected = requested.saturating_sub(already_connected);
    if let Err(error) = result {
        let _ = refresh_connection_graph(application);
        let message = application.tf(
            "status.easy_connect_failed",
            &[
                ("created", connected.to_string()),
                ("error", error.to_string()),
            ],
        );
        set_connection_feedback(application, message, true);
        return;
    }
    match refresh_connection_graph(application) {
        Ok(()) => {
            application.allow_patchbay_connections(&port_keys);
            application.sync_patchbay_connections();
            application.autosave_patchbay();
            let message = if connected == 0 {
                application.tf(
                    "status.easy_connect_existing",
                    &[("existing", already_connected.to_string())],
                )
            } else if already_connected == 0 {
                application.tf(
                    "status.easy_connect_summary",
                    &[("created", connected.to_string()), ("existing", "0".into())],
                )
            } else {
                application.tf(
                    "status.easy_connect_summary",
                    &[
                        ("created", connected.to_string()),
                        ("existing", already_connected.to_string()),
                    ],
                )
            };
            set_connection_feedback(application, message, false);
        }
        Err(error) => {
            let message = application.tf("status.easy_connect_refresh_failed", &[("error", error)]);
            set_connection_feedback(application, message, true);
        }
    }
}

fn pair_is_connectable(application: &Application, output: PortId, input: PortId) -> bool {
    let graph = application.source.graph();
    graph
        .port(output)
        .zip(graph.port(input))
        .is_some_and(|(output, input)| {
            application.source.node_connectable(output.node_id)
                && application.source.node_connectable(input.node_id)
        })
}

pub(crate) fn delete_selected_connections(application: &mut Application) {
    if !application.source.capabilities().disconnect {
        set_connection_feedback(
            application,
            application.t("status.connections_unavailable"),
            true,
        );
        return;
    }
    let keys = {
        let graph = application.source.graph();
        application
            .view
            .selected_links
            .iter()
            .filter_map(|id| {
                let link = graph.link(*id)?;
                if !application.source.is_link_mutable(link.id) {
                    return None;
                }
                Some((
                    graph.port_key(link.output_port)?,
                    graph.port_key(link.input_port)?,
                ))
            })
            .collect::<Vec<_>>()
    };
    if keys.is_empty() {
        let message = application.t("status.select_link_before_delete");
        set_connection_feedback(application, message, true);
        return;
    }

    let links: Vec<_> = application
        .source
        .graph()
        .links
        .values()
        .filter(|link| {
            application.source.is_link_mutable(link.id)
                && keys.iter().any(|(output, input)| {
                    application
                        .source
                        .graph()
                        .port_key(link.output_port)
                        .zip(application.source.graph().port_key(link.input_port))
                        .is_some_and(|pair| pair == (output.clone(), input.clone()))
                })
        })
        .cloned()
        .collect();
    let removed = links.len();
    if let Err(error) = application.commands.execute(
        Box::new(DisconnectManyCommand::from_links(
            application.source.graph(),
            links,
        )),
        &mut application.source,
    ) {
        let _ = refresh_connection_graph(application);
        let message = application.tf("status.disconnect_failed", &[("error", error.to_string())]);
        set_connection_feedback(application, message, true);
        return;
    }
    application.view.clear_selection();
    match refresh_connection_graph(application) {
        Ok(()) => {
            application.remove_patchbay_connections(&keys);
            application.sync_patchbay_connections();
            application.autosave_patchbay();
            let message = application.tf(
                "status.connections_removed",
                &[("count", removed.to_string())],
            );
            set_connection_feedback(application, message, false)
        }
        Err(error) => set_connection_feedback(
            application,
            application.tf(
                "status.connection_removed_refresh_failed",
                &[("error", error)],
            ),
            true,
        ),
    }
}

/// Disconnect every live link touching the currently selected node. This is
/// the framework-neutral equivalent of the old node context-menu command.
pub(crate) fn disconnect_selected_node(application: &mut Application) {
    if !application.source.capabilities().disconnect {
        set_connection_feedback(
            application,
            application.t("status.connections_unavailable"),
            true,
        );
        return;
    }
    let Some(node_id) = application.view.selected_nodes.iter().next().copied() else {
        let message = application.t("status.select_node_before_disconnect");
        set_connection_feedback(application, message, true);
        return;
    };
    let links = application
        .source
        .graph()
        .links
        .values()
        .filter(|link| {
            application.source.is_link_mutable(link.id)
                && application
                    .source
                    .graph()
                    .port(link.output_port)
                    .zip(application.source.graph().port(link.input_port))
                    .is_some_and(|(output, input)| {
                        output.node_id == node_id || input.node_id == node_id
                    })
        })
        .cloned()
        .collect::<Vec<_>>();
    if links.is_empty() {
        set_connection_feedback(
            application,
            application.t("status.no_links_for_node"),
            false,
        );
        return;
    }
    let keys = links
        .iter()
        .filter_map(|link| {
            application
                .source
                .graph()
                .port_key(link.output_port)
                .zip(application.source.graph().port_key(link.input_port))
        })
        .collect::<Vec<_>>();
    let count = links.len();
    match application.commands.execute(
        Box::new(DisconnectManyCommand::from_links(
            application.source.graph(),
            links,
        )),
        &mut application.source,
    ) {
        Ok(()) => match refresh_connection_graph(application) {
            Ok(()) => {
                application.remove_patchbay_connections(&keys);
                application.sync_patchbay_connections();
                application.autosave_patchbay();
                set_connection_feedback(
                    application,
                    application.tf("status.disconnected_all", &[("count", count.to_string())]),
                    false,
                );
            }
            Err(error) => set_connection_feedback(
                application,
                application.tf(
                    "status.connection_removed_refresh_failed",
                    &[("error", error)],
                ),
                true,
            ),
        },
        Err(error) => set_connection_feedback(
            application,
            application.tf("status.disconnect_failed", &[("error", error.to_string())]),
            true,
        ),
    }
}
