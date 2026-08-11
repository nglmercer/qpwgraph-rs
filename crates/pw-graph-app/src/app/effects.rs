use super::QpwgraphApp;
use pw_graph_backend::{
    BackendResult, EffectDriver, EffectInsertRequest, EffectInstance, EffectNodeRequest,
    GraphDriver,
};
use pw_graph_config::PersistedEffect;
use pw_graph_core::NodeId;
use pw_graph_effects::EffectDescriptor;
use pw_graph_ui::{EffectNodeControl, EffectNodeParameter};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static EFFECT_INSTANCE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Transient state for the effect gallery. It deliberately never aliases the
/// saved configuration, so dismissing the dialog cannot change an effect.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum EffectGalleryPhase {
    #[default]
    Choose,
    Configure,
}

impl EffectGalleryPhase {
    pub(crate) fn index(self) -> usize {
        match self {
            Self::Choose => 0,
            Self::Configure => 1,
        }
    }
}

/// The gallery deliberately stays a two-phase wizard for now. A future
/// routing phase can be added once link insertion has the same retained
/// component coverage as standalone effect creation.
#[derive(Clone, Debug)]
pub(crate) struct EffectGalleryState {
    pub(crate) phase: EffectGalleryPhase,
    pub(crate) effect_id: String,
    pub(crate) enabled: bool,
    pub(crate) parameters: BTreeMap<String, f32>,
    pub(crate) scroll_epoch: u32,
}

impl EffectGalleryState {
    fn new(descriptor: &EffectDescriptor, scroll_epoch: u32) -> Self {
        Self {
            phase: EffectGalleryPhase::Choose,
            effect_id: descriptor.id.clone(),
            enabled: true,
            parameters: default_parameters(descriptor),
            scroll_epoch,
        }
    }

    pub(crate) fn next_phase(&mut self) {
        if self.phase == EffectGalleryPhase::Choose {
            self.phase = EffectGalleryPhase::Configure;
            self.scroll_epoch = self.scroll_epoch.wrapping_add(1);
        }
    }

    pub(crate) fn previous_phase(&mut self) {
        if self.phase == EffectGalleryPhase::Configure {
            self.phase = EffectGalleryPhase::Choose;
            self.scroll_epoch = self.scroll_epoch.wrapping_add(1);
        }
    }
}

pub(crate) fn default_parameters(descriptor: &EffectDescriptor) -> BTreeMap<String, f32> {
    descriptor
        .parameters
        .iter()
        .map(|parameter| (parameter.id.clone(), parameter.default))
        .collect()
}

pub(crate) fn available_descriptors(driver: &dyn GraphDriver) -> Vec<EffectDescriptor> {
    let descriptors = driver.effect_descriptors();
    if descriptors.is_empty() {
        pw_graph_effects::EffectHost::new().descriptors()
    } else {
        descriptors
    }
}

impl QpwgraphApp {
    pub(crate) fn open_effect_gallery(&mut self) {
        let descriptors = available_descriptors(self.driver.as_ref());
        let Some(descriptor) = descriptors.first() else {
            self.status = self.t("effects.no_available");
            return;
        };
        self.show_shortcuts = false;
        self.show_history = false;
        self.show_preferences = false;
        self.effect_gallery_scroll_epoch = self.effect_gallery_scroll_epoch.wrapping_add(1);
        self.effect_gallery = Some(EffectGalleryState::new(
            descriptor,
            self.effect_gallery_scroll_epoch,
        ));
    }

    /// Copy backend effect metadata into the canvas model so effect nodes can
    /// render their controls without opening a second editor window.
    pub(crate) fn sync_effect_controls(&mut self) {
        let descriptors = available_descriptors(self.driver.as_ref());
        let controls = self
            .driver
            .effect_instances()
            .into_iter()
            .filter_map(|instance| {
                let descriptor = descriptors
                    .iter()
                    .find(|descriptor| descriptor.id == instance.config.effect_id)?;
                let parameters = descriptor
                    .parameters
                    .iter()
                    .map(|parameter| EffectNodeParameter {
                        id: parameter.id.clone(),
                        name: parameter.name.clone(),
                        minimum: parameter.minimum,
                        maximum: parameter.maximum,
                        value: instance
                            .config
                            .parameters
                            .get(&parameter.id)
                            .copied()
                            .unwrap_or(parameter.default),
                        unit: parameter.unit.clone(),
                        boolean: parameter.unit == "boolean",
                    })
                    .collect();
                Some((
                    instance.node_id,
                    EffectNodeControl {
                        enabled: instance.config.enabled,
                        parameters,
                    },
                ))
            })
            .collect();
        self.canvas.set_effect_controls(controls);
    }

    /// Apply the gallery state. Returns `true` only when an effect was added,
    /// allowing the modal caller to stay open and show an error on failure.
    pub(crate) fn create_effect_from_gallery(&mut self, gallery: &EffectGalleryState) -> bool {
        if !self.driver.supports_effect_nodes() {
            self.status = self.t("effects.backend_unavailable");
            return false;
        }
        let descriptors = available_descriptors(self.driver.as_ref());
        let Some(descriptor) = descriptors
            .iter()
            .find(|descriptor| descriptor.id == gallery.effect_id)
        else {
            self.status = self.t("effects.no_available");
            return false;
        };

        let instance_id = unique_effect_id();
        let result = self.driver.create_effect_node(EffectNodeRequest {
            instance_id,
            effect_id: descriptor.id.clone(),
            module_path: None,
            enabled: gallery.enabled,
            parameters: gallery.parameters.clone(),
            position: self.preferred_effect_position(),
        });

        match result {
            Ok(instance) => {
                self.persist_effect(instance);
                self.status = self.t("effects.created");
                self.canvas.clear_selected_link();
                true
            }
            Err(error) => {
                self.status = self.tf("effects.create_failed", &[("error", error.to_string())]);
                false
            }
        }
    }

    /// Resolves the effect node to its persisted driver instance and runs an
    /// update against the driver, applying `update` to the matching saved
    /// config entry on success. Both enabled and parameter writes share this
    /// lookup-and-persist shell so their failure reporting cannot drift.
    fn update_effect_for_node(
        &mut self,
        node_id: NodeId,
        apply: impl FnOnce(&mut dyn EffectDriver, &str) -> BackendResult<()>,
        persist: impl FnOnce(&mut PersistedEffect),
    ) {
        let Some(instance_id) = self.effect_instance_id(node_id) else {
            self.status = self.t("effects.not_found");
            return;
        };
        match apply(self.driver.as_mut(), &instance_id) {
            Ok(()) => {
                if let Some(saved) = self
                    .config
                    .effects
                    .iter_mut()
                    .find(|effect| effect.instance.instance_id == instance_id)
                {
                    persist(saved);
                }
            }
            Err(error) => self.status_error("effects.update_failed", &error),
        }
    }

    pub(crate) fn set_effect_enabled_for_node(&mut self, node_id: NodeId, enabled: bool) {
        self.update_effect_for_node(
            node_id,
            |driver, instance_id| driver.set_effect_enabled(instance_id, enabled),
            |saved| saved.instance.enabled = enabled,
        );
    }

    pub(crate) fn set_effect_parameter_for_node(
        &mut self,
        node_id: NodeId,
        parameter: &str,
        value: f32,
    ) {
        self.update_effect_for_node(
            node_id,
            |driver, instance_id| driver.set_effect_parameter(instance_id, parameter, value),
            |saved| {
                saved
                    .instance
                    .parameters
                    .insert(parameter.to_owned(), value);
            },
        );
    }

    pub(crate) fn remove_effect_node(&mut self, node_id: NodeId) {
        let Some(instance_id) = self.effect_instance_id(node_id) else {
            self.status = self.t("effects.not_found");
            return;
        };
        let saved_links = self.stable_link_pairs(&self.links_touching_node(node_id));
        match self.driver.remove_effect(&instance_id) {
            Ok(()) => {
                self.config
                    .effects
                    .retain(|effect| effect.instance.instance_id != instance_id);
                for (output, input) in saved_links {
                    self.patchbay.remove_stable_connection(&output, &input);
                }
                self.autosave_patchbay();
                self.status = self.t("effects.removed");
            }
            Err(error) => {
                self.status = self.tf("effects.remove_failed", &[("error", error.to_string())]);
            }
        }
    }

    /// Restore free-standing nodes before Patchbay activation. Their ports
    /// have no implicit route to rebuild, so they must exist while Patchbay
    /// resolves any saved manual connections to them.
    pub(crate) fn restore_standalone_effects(&mut self) {
        let saved = self
            .config
            .effects
            .iter()
            .filter(|effect| effect.source.is_none() && effect.destination.is_none())
            .cloned()
            .collect();
        self.restore_saved_effects(saved);
    }

    /// Restore routed insertions after Patchbay activation. An insertion
    /// deliberately replaces the direct source → destination link, so doing
    /// it first would let Patchbay recreate an unprocessed parallel route.
    pub(crate) fn restore_inserted_effects(&mut self) {
        let saved = self
            .config
            .effects
            .iter()
            .filter(|effect| effect.source.is_some() || effect.destination.is_some())
            .cloned()
            .collect();
        self.restore_saved_effects(saved);
    }

    fn restore_saved_effects(&mut self, saved: Vec<PersistedEffect>) {
        for effect in saved {
            let result = match (&effect.source, &effect.destination) {
                (Some(source), Some(destination)) => {
                    match self.driver.connect_by_key_if_missing(source, destination) {
                        Ok(_) => self.driver.insert_effect(EffectInsertRequest {
                            instance_id: effect.instance.instance_id.clone(),
                            effect_id: effect.instance.effect_id.clone(),
                            module_path: effect.instance.module_path.clone(),
                            source: source.clone(),
                            destination: destination.clone(),
                            enabled: effect.instance.enabled,
                            parameters: effect.instance.parameters.clone(),
                            position: effect.position,
                        }),
                        Err(error) => Err(error),
                    }
                }
                (None, None) => self.driver.create_effect_node(EffectNodeRequest {
                    instance_id: effect.instance.instance_id.clone(),
                    effect_id: effect.instance.effect_id.clone(),
                    module_path: effect.instance.module_path.clone(),
                    enabled: effect.instance.enabled,
                    parameters: effect.instance.parameters.clone(),
                    position: effect.position,
                }),
                _ => Err(pw_graph_backend::BackendError::Native(
                    "effect routing is incomplete".into(),
                )),
            };
            if let Err(error) = result {
                self.status = self.tf("effects.restore_failed", &[("error", error.to_string())]);
            }
        }
    }

    fn persist_effect(&mut self, instance: EffectInstance) {
        let position = self
            .driver
            .graph()
            .node(instance.node_id)
            .map(|node| node.position)
            .unwrap_or([260.0, 180.0]);
        self.config
            .effects
            .retain(|effect| effect.instance.instance_id != instance.config.instance_id);
        self.config.effects.push(PersistedEffect {
            instance: instance.config,
            source: instance.source,
            destination: instance.destination,
            position,
        });
    }

    fn effect_instance_id(&self, node_id: NodeId) -> Option<String> {
        self.driver
            .effect_instances()
            .into_iter()
            .find(|instance| instance.node_id == node_id)
            .map(|instance| instance.config.instance_id)
    }

    fn preferred_effect_position(&self) -> [f32; 2] {
        if let Some(node) = self
            .canvas
            .selected_node
            .and_then(|node_id| self.driver.graph().node(node_id))
        {
            return [node.position[0] + 290.0, node.position[1]];
        }
        let rightmost = self
            .driver
            .graph()
            .nodes
            .values()
            .map(|node| node.position[0])
            .fold(0.0_f32, f32::max);
        [rightmost + 290.0, 180.0]
    }
}

fn unique_effect_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let sequence = EFFECT_INSTANCE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("effect-{nanos:x}-{sequence:x}")
}

#[cfg(test)]
mod tests {
    use super::{default_parameters, EffectGalleryState};
    use pw_graph_effects::EffectHost;

    #[test]
    fn gallery_starts_with_a_standalone_node_and_default_parameters() {
        let descriptor = EffectHost::new().descriptors().remove(0);
        let state = EffectGalleryState::new(&descriptor, 4);
        assert_eq!(state.phase, super::EffectGalleryPhase::Choose);
        assert_eq!(state.parameters, default_parameters(&descriptor));
        assert_eq!(state.scroll_epoch, 4);
    }

    #[test]
    fn gallery_phases_reset_scroll_when_moving_between_steps() {
        let descriptor = EffectHost::new().descriptors().remove(0);
        let mut state = EffectGalleryState::new(&descriptor, 4);
        state.next_phase();
        assert_eq!(state.phase, super::EffectGalleryPhase::Configure);
        assert_eq!(state.scroll_epoch, 5);

        state.next_phase();
        assert_eq!(state.scroll_epoch, 5);

        state.previous_phase();
        assert_eq!(state.phase, super::EffectGalleryPhase::Choose);
        assert_eq!(state.scroll_epoch, 6);
    }
}
