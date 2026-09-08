use crate::source::ApplicationDriver;
use pw_graph_backend::{EffectInsertRequest, EffectInstance, EffectNodeRequest, GraphDriver};
use pw_graph_config::{AppConfig, PersistedEffect};
use pw_graph_effects::{EffectDescriptor, EffectParameter};
use pw_graph_i18n::I18n;
use slint::{Model, ModelRc, SharedString, VecModel};
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use super::app::Application;
use super::{EffectParameterRow, EffectRow, MainWindow};

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

pub(crate) fn restore_standalone_effects(
    source: &mut ApplicationDriver,
    config: &AppConfig,
    status: &mut String,
    i18n: &I18n,
) {
    let saved = config
        .effects
        .iter()
        .filter(|effect| effect.source.is_none() && effect.destination.is_none())
        .cloned()
        .collect::<Vec<_>>();
    restore_saved_effects(source, saved, status, i18n);
}

pub(crate) fn restore_inserted_effects(
    source: &mut ApplicationDriver,
    config: &AppConfig,
    status: &mut String,
    i18n: &I18n,
) {
    let saved = config
        .effects
        .iter()
        .filter(|effect| effect.source.is_some() && effect.destination.is_some())
        .cloned()
        .collect::<Vec<_>>();
    restore_saved_effects(source, saved, status, i18n);
}

fn restore_saved_effects(
    source: &mut ApplicationDriver,
    saved: Vec<PersistedEffect>,
    status: &mut String,
    i18n: &I18n,
) {
    if saved.is_empty() {
        return;
    }
    if !source.supports_effect_nodes() {
        status.push_str(" · ");
        status.push_str(&i18n.format(
            "status.restore_effects_unavailable",
            &[("count", saved.len().to_string())],
        ));
        return;
    }

    for saved in saved {
        let result = match (&saved.source, &saved.destination) {
            (Some(source_port), Some(destination_port)) => source
                .connect_by_key_if_missing(source_port, destination_port)
                .map(|_| ())
                .and_then(|_| {
                    source.insert_effect(EffectInsertRequest {
                        instance_id: saved.instance.instance_id.clone(),
                        effect_id: saved.instance.effect_id.clone(),
                        module_path: saved.instance.module_path.clone(),
                        source: source_port.clone(),
                        destination: destination_port.clone(),
                        enabled: saved.instance.enabled,
                        parameters: saved.instance.parameters.clone(),
                        position: saved.position,
                    })
                }),
            (None, None) => source.create_effect_node(EffectNodeRequest {
                instance_id: saved.instance.instance_id.clone(),
                effect_id: saved.instance.effect_id.clone(),
                module_path: saved.instance.module_path.clone(),
                enabled: saved.instance.enabled,
                parameters: saved.instance.parameters.clone(),
                position: saved.position,
            }),
            _ => Err("effect routing is incomplete".into()),
        };
        if let Err(error) = result {
            status.push_str(" · ");
            status.push_str(&i18n.format("status.restore_effect", &[("error", error)]));
        }
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
    let result = selected_link
        .and_then(|link| {
            application
                .source
                .graph()
                .port_key(link.output_port)
                .zip(application.source.graph().port_key(link.input_port))
                .map(|(source, destination)| {
                    application.source.insert_effect(EffectInsertRequest {
                        instance_id: instance_id.clone(),
                        effect_id: descriptor.id.clone(),
                        module_path: None,
                        source,
                        destination,
                        enabled,
                        parameters: parameters.clone(),
                        position,
                    })
                })
        })
        .unwrap_or_else(|| {
            application.source.create_effect_node(EffectNodeRequest {
                instance_id,
                effect_id: descriptor.id.clone(),
                module_path: None,
                enabled,
                parameters,
                position,
            })
        });
    match result {
        Ok(instance) => {
            let name = descriptor.name.clone();
            persist_effect(application, instance);
            match application.source.refresh() {
                Ok(()) => application.last_refresh = Instant::now(),
                Err(error) => {
                    application.status = application.tf(
                        "status.effect_refresh_failed",
                        &[("error", error.to_string())],
                    );
                    return;
                }
            }
            application.sync_patchbay_connections();
            application.autosave_patchbay();
            cancel_effect_setup(window, application);
            application.status = application.tf("status.effect_created", &[("name", name)]);
        }
        Err(error) => {
            application.status = application.tf("status.effect_create_failed", &[("error", error)])
        }
    }
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

pub(crate) fn cancel_effect_setup(window: &MainWindow, application: &mut Application) {
    application.effect_draft_id = None;
    application.effect_draft_enabled = true;
    application.effect_draft_parameters.clear();
    window.set_effect_configuring(false);
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
