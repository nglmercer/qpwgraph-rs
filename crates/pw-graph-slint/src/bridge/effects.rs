use crate::source::ApplicationDriver;
use pw_graph_backend::{
    EffectCreateRequest, EffectEvent, EffectInstance, EffectLoadStage, EffectTarget, GraphDriver,
};
use pw_graph_config::PersistedEffect;
use pw_graph_effects::{ChannelPolicy, EffectDescriptor, EffectParameter};
use pw_graph_i18n::I18n;
use slint::{Model, ModelRc, SharedString, VecModel};
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use super::app::{set_app_feedback, Application, PendingEffectUi};
use super::{EffectOperationRow, EffectParameterRow, EffectRow, MainWindow};

pub(crate) use super::effects_restore::{restore_inserted_effects, restore_standalone_effects};

fn default_parameters(descriptor: &EffectDescriptor) -> BTreeMap<String, f32> {
    descriptor
        .parameters
        .iter()
        .map(|parameter| (parameter.id.clone(), parameter.default))
        .collect()
}

fn available_descriptors(driver: &dyn GraphDriver) -> Vec<EffectDescriptor> {
    let descriptors = driver.effect_descriptors();
    if descriptors.is_empty() {
        pw_graph_effects::EffectHost::new().descriptors()
    } else {
        descriptors
    }
}

pub(crate) fn create_effect(window: &MainWindow, application: &mut Application) {
    if !application.source.supports_effect_nodes() {
        application.status = application.t("status.effect_processing_unavailable");
        return;
    }
    let descriptors = available_descriptors(&application.source);
    let Some(descriptor) = application
        .effect_selection_id
        .as_deref()
        .and_then(|id| descriptors.iter().find(|descriptor| descriptor.id == id))
        .or_else(|| {
            descriptors
                .iter()
                .find(|descriptor| descriptor.id == pw_graph_effects::DEFAULT_EFFECT_ID)
        })
        .or_else(|| descriptors.first())
        .cloned()
    else {
        application.status = application.t("status.no_effects_available");
        return;
    };

    if application.effect_draft_id.as_deref() != Some(descriptor.id.as_str())
        || !window.get_effect_configuring()
    {
        prepare_effect_draft(window, application);
        application.status = application.t("effects.setup_hint");
        return;
    }

    let instance_id = unique_effect_id(application);
    let parameters = application.effect_draft_parameters.clone();
    let enabled = application.effect_draft_enabled;
    let position = preferred_effect_position(application);
    let selected_link = application
        .view
        .selected_links
        .iter()
        .find_map(|id| application.source.graph().link(*id).cloned());
    let target = selected_link
        .and_then(|link| {
            application
                .source
                .graph()
                .port_key(link.output_port)
                .zip(application.source.graph().port_key(link.input_port))
                .map(|(source, destination)| EffectTarget::Insert {
                    source,
                    destination,
                    position,
                })
        })
        .unwrap_or(EffectTarget::Standalone { position });
    let result = application.source.begin_create_effect(EffectCreateRequest {
        instance_id,
        effect_id: descriptor.id.clone(),
        module_path: None,
        enabled,
        parameters,
        channel_policy: ChannelPolicy::Auto,
        target,
    });
    match result {
        Ok(ticket) => {
            let name = descriptor.name.clone();
            application.pending_effect_tickets.insert(
                ticket,
                PendingEffectUi {
                    ticket,
                    effect_id: descriptor.id.clone(),
                    effect_name: name.clone(),
                    stage: EffectLoadStage::Queued,
                    started_at: Instant::now(),
                    cancellable: true,
                },
            );
            finish_effect_setup(window, application);
            application.status = format!("{name}: loading (ticket {})", ticket.0);
        }
        Err(error) => {
            application.status = application.tf("status.effect_create_failed", &[("error", error)])
        }
    }
}

/// Consume backend lifecycle events on the UI/control thread.  A Ready event
/// is the first point at which a prepared effect is persisted and presented
/// as part of the graph; Failed preparation therefore leaves an inserted
/// route untouched.
pub(crate) fn poll_effect_events(application: &mut Application) -> bool {
    let events = match application.source.poll_effect_events() {
        Ok(events) => events,
        Err(error) => {
            set_app_feedback(
                application,
                application.tf("status.effect_create_failed", &[("error", error)]),
                true,
            );
            return true;
        }
    };
    let mut changed = false;
    for event in events {
        changed = true;
        let terminal_ticket = match &event {
            EffectEvent::Ready { ticket, .. }
            | EffectEvent::Failed { ticket, .. }
            | EffectEvent::Cancelled { ticket } => Some(*ticket),
            EffectEvent::Loading { .. } => None,
        };
        if let Some(ticket) = terminal_ticket {
            application.pending_effect_tickets.remove(&ticket);
        }
        match event {
            EffectEvent::Loading { ticket, stage } => {
                if let Some(operation) = application.pending_effect_tickets.get_mut(&ticket) {
                    operation.stage = stage;
                }
                application.status = format!("Effect ticket {}: {stage:?}", ticket.0);
            }
            EffectEvent::Ready { ticket, instance } => {
                let name = application
                    .source
                    .graph()
                    .node(instance.node_id)
                    .map(|node| node.name.clone())
                    .unwrap_or_else(|| instance.config.effect_id.clone());
                persist_effect(application, *instance);
                match application.source.refresh() {
                    Ok(()) => application.last_refresh = Instant::now(),
                    Err(error) => {
                        set_app_feedback(
                            application,
                            application.tf(
                                "status.effect_refresh_failed",
                                &[("error", error.to_string())],
                            ),
                            true,
                        );
                        continue;
                    }
                }
                application.sync_patchbay_connections();
                application.autosave_patchbay();
                set_app_feedback(
                    application,
                    format!("✓ {name} is ready (ticket {})", ticket.0),
                    false,
                );
            }
            EffectEvent::Failed { ticket, error } => {
                set_app_feedback(
                    application,
                    format!("✕ Effect ticket {} failed: {error}", ticket.0),
                    true,
                );
            }
            EffectEvent::Cancelled { ticket } => {
                set_app_feedback(
                    application,
                    format!("Effect ticket {} cancelled", ticket.0),
                    false,
                );
            }
        }
    }
    changed
}

pub(crate) fn toggle_effect(application: &mut Application, instance_id: &str) {
    let Some(instance) = application
        .source
        .effect_instances()
        .into_iter()
        .find(|instance| instance.config.instance_id == instance_id)
    else {
        application.status = application.tf(
            "status.effect_instance_not_found",
            &[("id", instance_id.to_owned())],
        );
        return;
    };
    let enabled = !instance.config.enabled;
    match application.source.set_effect_enabled(instance_id, enabled) {
        Ok(()) => {
            if let Some(saved) = application
                .config
                .effects
                .iter_mut()
                .find(|effect| effect.instance.instance_id == instance_id)
            {
                saved.instance.enabled = enabled;
            }
            application.status = application.tf(
                "status.effect_state",
                &[
                    ("id", instance_id.to_owned()),
                    (
                        "state",
                        application.t(if enabled {
                            "effects.enabled"
                        } else {
                            "effects.disabled"
                        }),
                    ),
                ],
            );
        }
        Err(error) => {
            application.status = application.tf("status.effect_state_failed", &[("error", error)])
        }
    }
}

pub(crate) fn set_effect_parameter_typed(
    application: &mut Application,
    instance_id: &str,
    parameter: &str,
    value: f32,
) {
    match application
        .source
        .set_effect_parameter(instance_id, parameter, value)
    {
        Ok(()) => {
            if let Some(saved) = application
                .config
                .effects
                .iter_mut()
                .find(|effect| effect.instance.instance_id == instance_id)
            {
                saved
                    .instance
                    .parameters
                    .insert(parameter.to_owned(), value);
            }
            application.status = application.tf(
                "status.effect_parameter_changed",
                &[
                    ("id", instance_id.to_owned()),
                    ("parameter", parameter.to_owned()),
                    ("value", format!("{value:.2}")),
                ],
            );
        }
        Err(error) => {
            application.status =
                application.tf("status.effect_parameter_failed", &[("error", error)])
        }
    }
}

pub(crate) fn remove_effect(application: &mut Application, instance_id: &str) {
    let effect_node_name = application
        .source
        .effect_instances()
        .into_iter()
        .find(|instance| instance.config.instance_id == instance_id)
        .and_then(|instance| {
            application
                .source
                .graph()
                .node(instance.node_id)
                .map(|node| node.name.clone())
        });
    let saved_pairs = application
        .config
        .effects
        .iter()
        .find(|effect| effect.instance.instance_id == instance_id)
        .and_then(|effect| effect.source.clone().zip(effect.destination.clone()));
    match application.source.remove_effect(instance_id) {
        Ok(()) => {
            application
                .config
                .effects
                .retain(|effect| effect.instance.instance_id != instance_id);
            if let Err(error) = application.source.refresh() {
                application.status =
                    application.tf("status.effect_removed_refresh_failed", &[("error", error)]);
            } else {
                application.last_refresh = Instant::now();
                application.sync_patchbay_connections();
                if let Some(effect_node_name) = effect_node_name {
                    application
                        .patchbay
                        .remove_connections_for_node(&effect_node_name);
                }
                if let Some((source, destination)) = saved_pairs {
                    application
                        .patchbay
                        .remove_stable_connection(&source, &destination);
                }
                application.autosave_patchbay();
                application.status =
                    application.tf("status.effect_removed", &[("id", instance_id.to_owned())]);
            }
        }
        Err(error) => {
            application.status = application.tf("status.effect_remove_failed", &[("error", error)])
        }
    }
}

pub(crate) fn open_effect_diagnostics(
    window: &MainWindow,
    application: &mut Application,
    instance_id: Option<&str>,
) {
    let instance = match instance_id {
        Some(instance_id) => application
            .source
            .effect_instances()
            .into_iter()
            .find(|instance| instance.config.instance_id == instance_id),
        None => application.source.effect_instances().into_iter().next(),
    };
    let Some(instance) = instance else {
        application.status = application.t("status.no_effect_instance");
        return;
    };
    let descriptor = application
        .source
        .effect_descriptors()
        .into_iter()
        .find(|descriptor| descriptor.id == instance.config.effect_id);
    let name = descriptor
        .as_ref()
        .map(|descriptor| descriptor.name.as_str())
        .unwrap_or(instance.config.effect_id.as_str());
    match application
        .source
        .effect_diagnostics(&instance.config.instance_id)
    {
        Ok(Some(report)) => {
            application.effect_debug_name = name.to_owned();
            application.effect_debug_health = instance.health.label().to_owned();
            application.effect_debug_report = report;
            window.set_show_effect_diagnostics(true);
        }
        Ok(None) => {
            application.status = application.t("status.effect_details_unavailable");
        }
        Err(error) => {
            application.status =
                application.tf("status.effect_details_failed", &[("error", error)]);
        }
    }
}

pub(crate) fn inspect_effect(application: &mut Application, instance_id: Option<&str>) {
    let instance = match instance_id {
        Some(instance_id) => application
            .source
            .effect_instances()
            .into_iter()
            .find(|instance| instance.config.instance_id == instance_id),
        None => application.source.effect_instances().into_iter().next(),
    };
    let Some(instance) = instance else {
        application.status = application.t("status.no_effect_instance");
        return;
    };
    let descriptor = application
        .source
        .effect_descriptors()
        .into_iter()
        .find(|descriptor| descriptor.id == instance.config.effect_id);
    let name = descriptor
        .as_ref()
        .map(|descriptor| descriptor.name.as_str())
        .unwrap_or(instance.config.effect_id.as_str());
    let parameters = instance
        .config
        .parameters
        .iter()
        .map(|(id, value)| format!("{id}={value:.2}"))
        .collect::<Vec<_>>()
        .join(", ");
    application.status = application.tf(
        if parameters.is_empty() {
            "status.effect_details"
        } else {
            "status.effect_details_with_parameters"
        },
        &[
            ("name", name.to_owned()),
            ("id", instance.config.instance_id.clone()),
            ("parameters", parameters),
        ],
    );
}

pub(crate) fn close_effect_diagnostics(window: &MainWindow, application: &mut Application) {
    window.set_show_effect_diagnostics(false);
    application.effect_debug_report.clear();
}

pub(crate) fn copy_effect_diagnostics(application: &mut Application) {
    if application.effect_debug_report.is_empty() {
        application.status = application.t("status.effect_details_unavailable");
    } else {
        application.status = application.t("status.effect_diagnostics_copied");
    }
}

pub(crate) fn effect_operation_rows(application: &Application) -> Vec<EffectOperationRow> {
    application
        .pending_effect_tickets
        .values()
        .map(|operation| EffectOperationRow {
            ticket: operation.ticket.0.min(i32::MAX as u64) as i32,
            name: SharedString::from(if operation.effect_name.is_empty() {
                operation.effect_id.clone()
            } else {
                operation.effect_name.clone()
            }),
            stage: SharedString::from(format!(
                "{:?} · {}s",
                operation.stage,
                operation.started_at.elapsed().as_secs()
            )),
            cancellable: operation.cancellable,
        })
        .collect()
}

fn persist_effect(application: &mut Application, instance: EffectInstance) {
    let position = application
        .source
        .graph()
        .node(instance.node_id)
        .map(|node| node.position)
        .unwrap_or([260.0, 180.0]);
    application
        .config
        .effects
        .retain(|effect| effect.instance.instance_id != instance.config.instance_id);
    application.config.effects.push(PersistedEffect {
        instance: instance.config,
        source: instance.source,
        destination: instance.destination,
        position,
    });
}

fn preferred_effect_position(application: &Application) -> [f32; 2] {
    let rightmost = application
        .source
        .graph()
        .nodes
        .values()
        .map(|node| node.position[0])
        .fold(0.0_f32, f32::max);
    [rightmost + 290.0, 180.0]
}

fn unique_effect_id(application: &Application) -> String {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    loop {
        let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let id = format!("slint-effect-{sequence}");
        if !application
            .source
            .effect_instances()
            .iter()
            .any(|effect| effect.config.instance_id == id)
            && !application
                .config
                .effects
                .iter()
                .any(|effect| effect.instance.instance_id == id)
        {
            return id;
        }
    }
}

pub(crate) fn effect_rows(source: &ApplicationDriver, i18n: &I18n) -> Vec<EffectRow> {
    let descriptors = available_descriptors(source);
    let mut instances = source.effect_instances();
    instances.sort_by(|a, b| a.config.instance_id.cmp(&b.config.instance_id));
    instances
        .into_iter()
        .map(|instance| {
            let descriptor = descriptors
                .iter()
                .find(|descriptor| descriptor.id == instance.config.effect_id);
            let name = descriptor
                .map(|descriptor| descriptor.name.clone())
                .unwrap_or_else(|| instance.config.effect_id.clone());
            let vendor = descriptor
                .map(|descriptor| descriptor.vendor.clone())
                .unwrap_or_else(|| i18n.text("effects.unknown_provider"));
            let description = instance.config.instance_id.clone();
            let vendor = match instance.error {
                Some(error) => {
                    i18n.format("effects.error", &[("vendor", vendor), ("error", error)])
                }
                None => vendor,
            };
            let diagnostics = instance.diagnostics.unwrap_or_default();
            let parameters = descriptor
                .map(|descriptor| {
                    descriptor
                        .parameters
                        .iter()
                        .map(|parameter| EffectParameterRow {
                            id: SharedString::from(parameter.id.clone()),
                            name: SharedString::from(parameter.name.clone()),
                            minimum: parameter.minimum,
                            maximum: parameter.maximum,
                            default_value: parameter.default,
                            value: instance
                                .config
                                .parameters
                                .get(&parameter.id)
                                .copied()
                                .unwrap_or(parameter.default),
                            unit: SharedString::from(parameter.unit.clone()),
                            boolean: parameter.unit == "boolean",
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            EffectRow {
                instance_id: SharedString::from(instance.config.instance_id.clone()),
                name: SharedString::from(name),
                vendor: SharedString::from(vendor),
                health: SharedString::from(instance.health.label()),
                diagnostics: SharedString::from(diagnostics),
                description: SharedString::from(description),
                enabled: instance.config.enabled,
                parameters: ModelRc::from(Rc::new(VecModel::from(parameters))),
            }
        })
        .collect()
}

/// Synchronize effect rows without replacing the outer or nested models when
/// the descriptor/instance shape is unchanged. Slint repeated components keep
/// pointer capture only while their model row survives; parameter values are
/// therefore patched into the existing `VecModel` during slider drags.
pub(crate) fn sync_effect_rows(
    current: ModelRc<EffectRow>,
    source: &ApplicationDriver,
    i18n: &I18n,
) -> Option<ModelRc<EffectRow>> {
    let descriptors = available_descriptors(source);
    let mut instances = source.effect_instances();
    instances.sort_by(|a, b| a.config.instance_id.cmp(&b.config.instance_id));
    let Some(model) = current.as_any().downcast_ref::<VecModel<EffectRow>>() else {
        return Some(ModelRc::from(Rc::new(VecModel::from(effect_rows(
            source, i18n,
        )))));
    };
    if model.row_count() != instances.len()
        || instances.iter().enumerate().any(|(index, instance)| {
            model
                .row_data(index)
                .is_none_or(|row| row.instance_id.as_str() != instance.config.instance_id)
        })
    {
        return Some(ModelRc::from(Rc::new(VecModel::from(effect_rows(
            source, i18n,
        )))));
    }

    for (index, instance) in instances.into_iter().enumerate() {
        let Some(current_row) = model.row_data(index) else {
            continue;
        };
        let descriptor = descriptors
            .iter()
            .find(|descriptor| descriptor.id == instance.config.effect_id);
        let name = descriptor
            .map(|descriptor| descriptor.name.clone())
            .unwrap_or_else(|| instance.config.effect_id.clone());
        let vendor = descriptor
            .map(|descriptor| descriptor.vendor.clone())
            .unwrap_or_else(|| i18n.text("effects.unknown_provider"));
        let vendor = match instance.error {
            Some(error) => i18n.format("effects.error", &[("vendor", vendor), ("error", error)]),
            None => vendor,
        };
        let diagnostics = instance.diagnostics.unwrap_or_default();
        let parameter_rows = descriptor
            .map(|descriptor| {
                descriptor
                    .parameters
                    .iter()
                    .map(|parameter| EffectParameterRow {
                        id: SharedString::from(parameter.id.clone()),
                        name: SharedString::from(parameter.name.clone()),
                        minimum: parameter.minimum,
                        maximum: parameter.maximum,
                        default_value: parameter.default,
                        value: instance
                            .config
                            .parameters
                            .get(&parameter.id)
                            .copied()
                            .unwrap_or(parameter.default),
                        unit: SharedString::from(parameter.unit.clone()),
                        boolean: parameter.unit == "boolean",
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let parameters = if let Some(parameter_model) = current_row
            .parameters
            .as_any()
            .downcast_ref::<VecModel<EffectParameterRow>>()
        {
            let stable_shape = parameter_model.row_count() == parameter_rows.len()
                && parameter_rows
                    .iter()
                    .enumerate()
                    .all(|(parameter_index, row)| {
                        parameter_model
                            .row_data(parameter_index)
                            .is_some_and(|current| current.id == row.id)
                    });
            if stable_shape {
                for (parameter_index, row) in parameter_rows.into_iter().enumerate() {
                    if parameter_model
                        .row_data(parameter_index)
                        .is_some_and(|current| current == row)
                    {
                        continue;
                    }
                    parameter_model.set_row_data(parameter_index, row);
                }
                current_row.parameters.clone()
            } else {
                ModelRc::from(Rc::new(VecModel::from(parameter_rows)))
            }
        } else {
            ModelRc::from(Rc::new(VecModel::from(parameter_rows)))
        };
        let next = EffectRow {
            instance_id: SharedString::from(instance.config.instance_id),
            name: SharedString::from(name),
            vendor: SharedString::from(vendor),
            health: SharedString::from(instance.health.label()),
            diagnostics: SharedString::from(diagnostics),
            description: current_row.description.clone(),
            enabled: instance.config.enabled,
            parameters,
        };
        if current_row != next {
            model.set_row_data(index, next);
        }
    }
    None
}

pub(crate) fn effect_options(source: &ApplicationDriver) -> Vec<SharedString> {
    available_descriptors(source)
        .into_iter()
        .map(|descriptor| SharedString::from(descriptor.name))
        .collect()
}

fn parameter_row(parameter: &EffectParameter, value: f32) -> EffectParameterRow {
    EffectParameterRow {
        id: SharedString::from(parameter.id.clone()),
        name: SharedString::from(parameter.name.clone()),
        minimum: parameter.minimum,
        maximum: parameter.maximum,
        default_value: parameter.default,
        value,
        unit: SharedString::from(parameter.unit.clone()),
        boolean: parameter.unit == "boolean",
    }
}

pub(crate) fn effect_setup_rows(
    source: &ApplicationDriver,
    effect_id: Option<&str>,
    values: &BTreeMap<String, f32>,
) -> Vec<EffectParameterRow> {
    let Some(effect_id) = effect_id else {
        return Vec::new();
    };
    available_descriptors(source)
        .into_iter()
        .find(|descriptor| descriptor.id == effect_id)
        .map(|descriptor| {
            descriptor
                .parameters
                .iter()
                .map(|parameter| {
                    parameter_row(
                        parameter,
                        values
                            .get(&parameter.id)
                            .copied()
                            .unwrap_or(parameter.default),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Update the draft setup model without replacing it during a slider drag.
/// Slint's repeated controls keep pointer capture only while their model row
/// survives, so value changes are patched into the existing `VecModel` when
/// the parameter structure is unchanged.
pub(crate) fn sync_effect_setup_rows(
    current: ModelRc<EffectParameterRow>,
    rows: Vec<EffectParameterRow>,
) -> Option<ModelRc<EffectParameterRow>> {
    let Some(model) = current
        .as_any()
        .downcast_ref::<VecModel<EffectParameterRow>>()
    else {
        return Some(ModelRc::from(Rc::new(VecModel::from(rows))));
    };
    let stable_shape = model.row_count() == rows.len()
        && rows.iter().enumerate().all(|(index, row)| {
            model
                .row_data(index)
                .is_some_and(|current| current.id == row.id)
        });
    if !stable_shape {
        return Some(ModelRc::from(Rc::new(VecModel::from(rows))));
    }
    for (index, row) in rows.into_iter().enumerate() {
        if model.row_data(index).is_some_and(|current| current == row) {
            continue;
        }
        model.set_row_data(index, row);
    }
    None
}

pub(crate) fn prepare_effect_draft(window: &MainWindow, application: &mut Application) {
    let descriptors = available_descriptors(&application.source);
    let descriptor = application
        .effect_selection_id
        .as_deref()
        .and_then(|id| descriptors.iter().find(|descriptor| descriptor.id == id))
        .cloned()
        .or_else(|| {
            descriptors
                .iter()
                .find(|descriptor| descriptor.id == pw_graph_effects::DEFAULT_EFFECT_ID)
                .cloned()
        })
        .or_else(|| descriptors.first().cloned());
    let Some(descriptor) = descriptor else {
        application.effect_draft_id = None;
        application.effect_draft_parameters.clear();
        window.set_effect_configuring(false);
        application.status = application.t("status.no_effects_available");
        return;
    };
    let index = descriptors
        .iter()
        .position(|candidate| candidate.id == descriptor.id)
        .unwrap_or(0);
    application.effect_draft_id = Some(descriptor.id.clone());
    application.effect_selection_id = Some(descriptor.id.clone());
    application.effect_draft_enabled = true;
    application.effect_draft_parameters = default_parameters(&descriptor);
    window.set_effect_selection_index(index as i32);
    window.set_effect_configuring(true);
}

pub(crate) fn discard_effect_draft(window: &MainWindow, application: &mut Application) {
    finish_effect_setup(window, application);
}

/// Compatibility name for callers that used the old setup-only operation.
/// It now discards only the unsubmitted draft; submitted tickets are owned by
/// the background lifecycle until explicit cancellation or shutdown.
pub(crate) fn cancel_effect_setup(window: &MainWindow, application: &mut Application) {
    discard_effect_draft(window, application);
}

/// Close the setup form after a successful queue operation. This must not
/// cancel the ticket that was just returned: preparation continues while the
/// dialog is closed and the control pump will publish its terminal event.
fn finish_effect_setup(window: &MainWindow, application: &mut Application) {
    application.effect_draft_id = None;
    application.effect_draft_enabled = true;
    application.effect_draft_parameters.clear();
    window.set_effect_configuring(false);
}

/// Cancel only user-created pending tickets. Restore tickets are owned by the
/// backend restore lifecycle and are deliberately allowed to finish in the
/// background. A race with a terminal event is harmless; the backend reports
/// that ticket as completed and the next pump removes it from this set.
pub(crate) fn cancel_pending_effects(application: &mut Application) {
    let tickets: Vec<_> = application
        .pending_effect_tickets
        .values()
        .filter(|operation| operation.cancellable)
        .map(|operation| operation.ticket)
        .collect();
    for ticket in tickets {
        if let Some(operation) = application.pending_effect_tickets.get_mut(&ticket) {
            operation.cancellable = false;
        }
        if let Err(error) = application.source.cancel_effect(ticket) {
            application.status =
                format!("Effect ticket {} could not be cancelled: {error}", ticket.0);
        }
    }
}

pub(crate) fn cancel_effect_ticket(application: &mut Application, ticket: u64) {
    let ticket = pw_graph_backend::EffectTicket(ticket);
    let Some(operation) = application.pending_effect_tickets.get(&ticket) else {
        return;
    };
    if !operation.cancellable {
        return;
    }
    let effect_name = operation.effect_name.clone();
    if let Err(error) = application.source.cancel_effect(ticket) {
        application.status = format!("Effect ticket {} could not be cancelled: {error}", ticket.0);
    } else {
        if let Some(operation) = application.pending_effect_tickets.get_mut(&ticket) {
            operation.cancellable = false;
        }
        application.status = format!("{effect_name}: cancelling");
    }
}

pub(crate) fn select_effect_draft(
    window: &MainWindow,
    application: &mut Application,
    index: usize,
) {
    let descriptors = available_descriptors(&application.source);
    if let Some(descriptor) = descriptors.get(index) {
        application.effect_selection_id = Some(descriptor.id.clone());
    }
    window.set_effect_selection_index(index as i32);
    prepare_effect_draft(window, application);
}

pub(crate) fn set_effect_draft_enabled(application: &mut Application, enabled: bool) {
    if application.effect_draft_id.is_some() {
        application.effect_draft_enabled = enabled;
    }
}

pub(crate) fn set_effect_draft_parameter_typed(
    application: &mut Application,
    parameter_id: &str,
    value: f32,
) {
    let Some(effect_id) = application.effect_draft_id.as_deref() else {
        return;
    };
    let Some(parameter) = available_descriptors(&application.source)
        .into_iter()
        .find(|descriptor| descriptor.id == effect_id)
        .and_then(|descriptor| {
            descriptor
                .parameters
                .into_iter()
                .find(|parameter| parameter.id == parameter_id)
        })
    else {
        application.status = application.t("status.effect_parameter_invalid");
        return;
    };
    application.effect_draft_parameters.insert(
        parameter.id,
        if parameter.unit == "boolean" {
            if value >= 0.5 {
                1.0
            } else {
                0.0
            }
        } else {
            value.clamp(parameter.minimum, parameter.maximum)
        },
    );
}
