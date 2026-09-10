//! Patchbay state and file operations shared by every Slint action.

use super::app::Application;
use super::models::{profile_options, selected_patchbay_path};
use super::MainWindow;
use pw_graph_core::{
    Direction, EndpointMatchMode, EndpointResolution, EndpointSelector, NodeIdentity, NodeType,
    PortId, PortType,
};
use pw_graph_i18n::I18n;
use rfd::FileDialog;
use std::path::PathBuf;

pub(crate) fn select_patchbay_path(application: &mut Application, path: PathBuf) {
    application.patchbay_file = path.clone();
    application.config.patchbay_dir = path.parent().map(PathBuf::from);
    application
        .config
        .recent_patchbay_paths
        .retain(|item| item != &path);
    application
        .config
        .recent_patchbay_paths
        .insert(0, path.clone());
    application.config.recent_patchbay_paths.truncate(8);
    application.config.patchbay_path = Some(path.clone());
    application
        .config
        .patchbay_profiles
        .insert(application.config.active_patchbay_profile.clone(), path);
}

pub(crate) fn save_patchbay(application: &mut Application) {
    let filter = application.t("patchbay.file_filter");
    let directory = application
        .config
        .patchbay_dir
        .clone()
        .or_else(|| application.patchbay_file.parent().map(PathBuf::from));
    let selected = FileDialog::new()
        .set_directory(directory.unwrap_or_else(|| PathBuf::from(".")))
        .set_file_name(
            application
                .patchbay_file
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("default.qpwgraph"),
        )
        .add_filter(&filter, &["qpwgraph", "xml", "json"])
        .save_file();
    let Some(path) = selected else {
        return;
    };
    select_patchbay_path(application, path);
    match application.patchbay.save_to(&application.patchbay_file) {
        Ok(()) => {
            application.status = application.tf(
                "status.saved_patchbay",
                &[("path", application.patchbay_file.display().to_string())],
            );
        }
        Err(error) => {
            application.status = application.tf(
                "status.patchbay_save_failed",
                &[("error", error.to_string())],
            );
        }
    }
}

pub(crate) fn load_patchbay(application: &mut Application) {
    let filter = application.t("patchbay.file_filter");
    let directory = application
        .config
        .patchbay_dir
        .clone()
        .or_else(|| application.patchbay_file.parent().map(PathBuf::from));
    let selected = FileDialog::new()
        .set_directory(directory.unwrap_or_else(|| PathBuf::from(".")))
        .add_filter(&filter, &["qpwgraph", "xml", "json"])
        .pick_file();
    let Some(path) = selected else {
        return;
    };
    load_patchbay_path(application, path);
}

pub(crate) fn load_patchbay_path(application: &mut Application, path: PathBuf) -> bool {
    match pw_graph_patchbay::Patchbay::load_from(&path) {
        Ok(patchbay) => {
            select_patchbay_path(application, path);
            application.patchbay = patchbay;
            if application.config.patchbay_activated {
                activate_patchbay(application);
            }
            application.status = application.tf(
                "status.loaded",
                &[("path", application.patchbay_file.display().to_string())],
            );
            true
        }
        Err(error) => {
            application.status = application.tf(
                "status.patchbay_load_failed",
                &[("error", error.to_string())],
            );
            false
        }
    }
}

pub(crate) fn select_profile(window: &MainWindow, application: &mut Application, index: usize) {
    let profiles = profile_options(&application.config);
    let Some(profile) = profiles.get(index).cloned() else {
        application.status = application.t("status.profile_not_found");
        return;
    };
    let path = application
        .config
        .patchbay_profiles
        .get(&profile)
        .cloned()
        .unwrap_or_else(|| {
            if profile == application.config.active_patchbay_profile {
                selected_patchbay_path(&application.config)
            } else {
                application
                    .config
                    .patchbay_dir
                    .clone()
                    .unwrap_or_else(|| PathBuf::from("."))
                    .join(format!("{profile}.qpwgraph"))
            }
        });
    let previous_profile = application.config.active_patchbay_profile.clone();
    application.config.active_patchbay_profile = profile.clone();
    window.set_profile_name(profile.clone().into());
    if load_patchbay_path(application, path) {
        application.status = application.tf("status.profile_selected", &[("profile", profile)]);
    } else {
        application.config.active_patchbay_profile = previous_profile.clone();
        window.set_profile_name(previous_profile.into());
    }
}

pub(crate) fn load_recent_patchbay(application: &mut Application, index: usize) {
    let Some(path) = application.config.recent_patchbay_paths.get(index).cloned() else {
        application.status = application.t("status.recent_file_not_found");
        return;
    };
    load_patchbay_path(application, path);
}

pub(crate) fn choose_patchbay_directory(application: &mut Application) {
    let initial = application
        .config
        .patchbay_dir
        .clone()
        .or_else(|| application.patchbay_file.parent().map(PathBuf::from));
    if let Some(path) = FileDialog::new()
        .set_directory(initial.unwrap_or_else(|| PathBuf::from(".")))
        .pick_folder()
    {
        application.config.patchbay_dir = Some(path);
        application.status = application.t("status.patchbay_directory_changed");
    }
}

pub(crate) fn save_profile(application: &mut Application) {
    let profile = application.config.active_patchbay_profile.trim().to_owned();
    if profile.is_empty() {
        application.status = application.t("status.profile_name_required");
        return;
    }
    application
        .config
        .patchbay_profiles
        .insert(profile, application.patchbay_file.clone());
    application.autosave_patchbay();
    application.status = application.t("status.profile_saved");
}

pub(crate) fn activate_patchbay(application: &mut Application) {
    application.config.patchbay_activated = true;
    match application.patchbay.activate(
        &mut application.source,
        application.config.patchbay_exclusive,
        application.config.patchbay_auto_disconnect,
    ) {
        Ok(report) => {
            application
                .patchbay_reconciler
                .schedule_now(std::time::Instant::now());
            application.sync_patchbay_connections();
            application.autosave_patchbay();
            application.status = application.tf(
                "status.activated",
                &[
                    ("connected", report.connected.to_string()),
                    ("present", report.already_present.to_string()),
                    ("disconnected", report.disconnected.to_string()),
                ],
            );
            if !report.failed.is_empty() {
                application.status.push_str(" · ");
                application.status.push_str(&report.failed.join("; "));
            }
            if !report.waiting.is_empty() {
                application
                    .status
                    .push_str(" · waiting for saved endpoints");
            }
            if !report.ambiguous.is_empty() {
                application
                    .status
                    .push_str(" · saved endpoint is ambiguous");
            }
        }
        Err(error) => {
            application.status =
                application.tf("status.activation_failed", &[("error", error.to_string())]);
        }
    }
}

pub(crate) fn open_patchbay_diagnostics(window: &super::MainWindow, application: &mut Application) {
    application.patchbay_debug_report = application.patchbay.debug_report(
        application.source.graph(),
        application.patchbay_reconciler.last_report(),
    );
    window.set_patchbay_debug_report(application.patchbay_debug_report.clone().into());
    window.set_show_patchbay_diagnostics(true);
}

pub(crate) fn close_patchbay_diagnostics(
    window: &super::MainWindow,
    application: &mut Application,
) {
    window.set_show_patchbay_diagnostics(false);
    application.patchbay_debug_report.clear();
}

pub(crate) fn copy_patchbay_diagnostics(application: &mut Application) {
    application.status = application.t("status.patchbay_diagnostics_copied");
}

pub(crate) fn restore_effect_connections(
    source: &mut super::super::source::ApplicationDriver,
    patchbay: &pw_graph_patchbay::Patchbay,
    status: &mut String,
    i18n: &I18n,
) {
    let effect_patchbay = patchbay.effect_connections();
    if effect_patchbay.connections.is_empty() {
        return;
    }
    match effect_patchbay.activate(source, false, false) {
        Ok(report) if report.failed.is_empty() => {}
        Ok(report) => status.push_str(&format!(
            " · {}",
            i18n.format(
                "status.effect_restore_routes_failed",
                &[("error", report.failed.join("; "))],
            )
        )),
        Err(error) => status.push_str(&format!(
            " · {}",
            i18n.format(
                "status.effect_restore_routes_failed",
                &[("error", error.to_string())]
            )
        )),
    }
}

pub(crate) fn snapshot_patchbay(application: &mut Application) {
    application
        .patchbay
        .snapshot_driver(&application.source, application.config.patchbay_auto_pin);
    application.autosave_patchbay();
    application.status = application.tf(
        "status.snapshot",
        &[("count", application.patchbay.connections.len().to_string())],
    );
}

pub(crate) fn add_rule_from_selection(window: &MainWindow, application: &mut Application) {
    let selected: Vec<_> = application
        .view
        .selected_links
        .iter()
        .filter_map(|id| application.source.graph().link(*id).cloned())
        .filter(|link| application.source.is_link_mutable(link.id))
        .collect();
    if selected.is_empty() {
        if !application.view.selected_links.is_empty() {
            application.status = application.t("status.connections_unavailable");
            return;
        }
        application.patchbay.connections.push(Default::default());
        let index = application.patchbay.connections.len() - 1;
        begin_rule_edit(window, application, index);
        application.status = application.t("patchbay.new_rule");
        return;
    }
    for link in selected {
        application.patchbay.add_graph_connection(
            application.source.graph(),
            link.output_port,
            link.input_port,
            application.config.patchbay_auto_pin,
        );
    }
    application.autosave_patchbay();
    application.status = application.t("status.rule_added");
}

pub(crate) fn begin_rule_edit(window: &MainWindow, application: &mut Application, index: usize) {
    let Some(rule) = application.patchbay.connections.get(index) else {
        application.status = application.t("status.rule_not_found");
        return;
    };
    window.set_rule_editor_index(index as i32);
    window.set_rule_output_node(rule.output_node.clone().into());
    window.set_rule_output_port(rule.output_name.clone().into());
    window.set_rule_input_node(rule.input_node.clone().into());
    window.set_rule_input_port(rule.input_name.clone().into());
}

pub(crate) fn save_rule(window: &MainWindow, application: &mut Application) {
    let index = window.get_rule_editor_index();
    let index = index.max(0) as usize;
    if application.patchbay.connections.get(index).is_none() {
        application.status = application.t("status.rule_not_found");
        return;
    }
    let output_node = window.get_rule_output_node().trim().to_owned();
    let output_port = window.get_rule_output_port().trim().to_owned();
    let input_node = window.get_rule_input_node().trim().to_owned();
    let input_port = window.get_rule_input_port().trim().to_owned();
    if output_node.is_empty()
        || output_port.is_empty()
        || input_node.is_empty()
        || input_port.is_empty()
    {
        application.status = application.t("status.rule_invalid");
        return;
    }

    let graph = application.source.graph();
    let output = match resolve_named_endpoint(graph, &output_node, &output_port, Direction::Source)
    {
        Ok(endpoint) => endpoint,
        Err(EndpointResolution::Ambiguous(candidates)) => {
            application.status = application.tf(
                "status.rule_endpoint_ambiguous",
                &[("count", candidates.len().to_string())],
            );
            return;
        }
        Err(_) => {
            application.status = application.t("status.rule_endpoint_not_found");
            return;
        }
    };
    let input = match resolve_named_endpoint(graph, &input_node, &input_port, Direction::Sink) {
        Ok(endpoint) => endpoint,
        Err(EndpointResolution::Ambiguous(candidates)) => {
            application.status = application.tf(
                "status.rule_endpoint_ambiguous",
                &[("count", candidates.len().to_string())],
            );
            return;
        }
        Err(_) => {
            application.status = application.t("status.rule_endpoint_not_found");
            return;
        }
    };
    let (output_id, output_node_type, output_type) = output;
    let (input_id, input_node_type, input_type) = input;
    if !(output_type == input_type
        || output_type == PortType::Unknown
        || input_type == PortType::Unknown)
    {
        application.status = application.t("status.rule_endpoint_incompatible");
        return;
    }
    let output_selector = graph.port_key(output_id).map(|key| key.selector());
    let input_selector = graph.port_key(input_id).map(|key| key.selector());

    let rule = application
        .patchbay
        .connections
        .get_mut(index)
        .expect("rule was checked above");
    rule.output_node = output_node;
    rule.output_name = output_port;
    rule.input_node = input_node;
    rule.input_name = input_port;
    rule.output_port = output_id;
    rule.input_port = input_id;
    rule.node_type = output_node_type;
    rule.output_node_type = Some(output_node_type);
    rule.input_node_type = Some(input_node_type);
    rule.port_type = output_type;
    rule.output_selector = output_selector;
    rule.input_selector = input_selector;
    window.set_rule_editor_index(-1);
    application.autosave_patchbay();
    if application.config.patchbay_activated {
        activate_patchbay(application);
    }
    application.status = application.t("status.rule_saved");
}

pub(crate) fn cancel_rule_edit(window: &MainWindow, application: &mut Application) {
    let index = window.get_rule_editor_index().max(0) as usize;
    if application
        .patchbay
        .connections
        .get(index)
        .is_some_and(|rule| {
            rule.output_node.is_empty()
                && rule.output_name.is_empty()
                && rule.input_node.is_empty()
                && rule.input_name.is_empty()
        })
    {
        application.patchbay.connections.remove(index);
    }
    window.set_rule_editor_index(-1);
}

fn resolve_named_endpoint(
    graph: &pw_graph_core::Graph,
    node_name: &str,
    port_name: &str,
    direction: Direction,
) -> Result<(PortId, NodeType, PortType), EndpointResolution> {
    let selector = EndpointSelector {
        node_type: NodeType::Unknown,
        identity: NodeIdentity::with_node_name(node_name),
        port_name: port_name.to_owned(),
        channel: None,
        direction,
        port_type: PortType::Unknown,
        match_mode: EndpointMatchMode::Instance,
    };
    let resolution = graph.resolve_endpoint(&selector);
    let port_id = resolution.port_id().ok_or(resolution)?;
    let port = graph.port(port_id).ok_or(EndpointResolution::Missing)?;
    let node = graph
        .node(port.node_id)
        .ok_or(EndpointResolution::Missing)?;
    Ok((port_id, node.node_type, port.port_type))
}

pub(crate) fn remove_rule(application: &mut Application, index: usize) {
    if index >= application.patchbay.connections.len() {
        application.status = application.t("status.rule_not_found");
        return;
    }
    application.patchbay.connections.remove(index);
    application.autosave_patchbay();
    application.status = application.t("status.rule_removed");
}

pub(crate) fn toggle_rule_pin(application: &mut Application, index: usize) {
    let Some(rule) = application.patchbay.connections.get_mut(index) else {
        application.status = application.t("status.rule_not_found");
        return;
    };
    rule.pinned = !rule.pinned;
    application.autosave_patchbay();
    application.status = application.t("status.rule_pin_changed");
}
