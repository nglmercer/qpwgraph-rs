//! Persisted effect restoration and insertion-target reconstruction.

use crate::source::ApplicationDriver;
use pw_graph_backend::{EffectCreateRequest, EffectTarget};
use pw_graph_config::{AppConfig, PersistedEffect};
use pw_graph_i18n::I18n;

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
                    source
                        .begin_create_effect(EffectCreateRequest {
                            instance_id: saved.instance.instance_id.clone(),
                            effect_id: saved.instance.effect_id.clone(),
                            module_path: saved.instance.module_path.clone(),
                            enabled: saved.instance.enabled,
                            parameters: saved.instance.parameters.clone(),
                            channel_policy: saved.instance.channel_policy,
                            target: EffectTarget::Insert {
                                source: source_port.clone(),
                                destination: destination_port.clone(),
                                position: saved.position,
                            },
                        })
                        .map(|_| ())
                }),
            (None, None) => source
                .begin_create_effect(EffectCreateRequest {
                    instance_id: saved.instance.instance_id.clone(),
                    effect_id: saved.instance.effect_id.clone(),
                    module_path: saved.instance.module_path.clone(),
                    enabled: saved.instance.enabled,
                    parameters: saved.instance.parameters.clone(),
                    channel_policy: saved.instance.channel_policy,
                    target: EffectTarget::Standalone {
                        position: saved.position,
                    },
                })
                .map(|_| ()),
            _ => Err("effect routing is incomplete".into()),
        };
        if let Err(error) = result {
            status.push_str(" · ");
            status.push_str(&i18n.format("status.restore_effect", &[("error", error)]));
        }
    }
}
