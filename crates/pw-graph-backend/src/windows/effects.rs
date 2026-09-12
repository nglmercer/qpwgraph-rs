//! Effect nodes on Windows.
//!
//! An effect is a graph node with one audio input port and one audio output
//! port, and a processor in the router between them. Linking into its input
//! and out of its output is an ordinary graph operation — [`super::routing`]
//! walks the links and folds any effects it passes through into the route's
//! chain — so nothing here has to special-case routing, and an effect works
//! the same whether the user dropped it on the canvas and wired it up by hand
//! or asked for it to be inserted into an existing link.
//!
//! Insertion is the one composite operation: cut the chosen link, put the
//! effect in the gap, and remember the original endpoints so removal can put
//! the link back. If any step fails, the original link is restored and the
//! node is destroyed, because an effect that half-inserted itself would leave
//! audio going nowhere.
//!
//! The instance id is the caller's, and it is what survives a restart. Node
//! and port ids are derived from it, so a restored effect lands on the same
//! pins the patchbay file remembers.

use super::*;

use crate::api::{EffectCreateRequest, EffectInsertRequest, EffectInstance, EffectNodeRequest};

use pw_graph_effects::{
    EffectComponentManager, EffectDescriptor, EffectHost, EffectInstanceConfig,
    EffectPreparationEvent, EffectPrepareRequest, EffectTicket, PreparedEffect,
};

/// Effect instances this driver owns, and the factory that builds them.
///
/// Kept next to the graph rather than inside the router: the router knows
/// about processors and chains, not about nodes, ports, or the identity a
/// patchbay file will use to find this effect again tomorrow.
pub(super) struct WindowsEffects {
    host: EffectHost,
    loader: EffectComponentManager,
    pending: BTreeMap<EffectTicket, EffectCreateRequest>,
    instances: BTreeMap<String, EffectInstance>,
    /// Effects owned by a persisted Windows application route are kept out
    /// of the public standalone-effect list. Their complete configuration
    /// lives on the route itself, and exposing them as independent UI effects
    /// would cause them to be persisted twice.
    route_instances: BTreeSet<String>,
}

impl std::fmt::Debug for WindowsEffects {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WindowsEffects")
            .field("instances", &self.instances.len())
            .finish()
    }
}

impl Default for WindowsEffects {
    fn default() -> Self {
        Self::new()
    }
}

impl WindowsEffects {
    pub(super) fn new() -> Self {
        Self {
            host: EffectHost::new(),
            loader: EffectComponentManager::new(2, 16),
            pending: BTreeMap::new(),
            instances: BTreeMap::new(),
            route_instances: BTreeSet::new(),
        }
    }

    pub(super) fn begin_create_effect(
        &mut self,
        request: EffectCreateRequest,
    ) -> BackendResult<EffectTicket> {
        if self.instances.contains_key(&request.instance_id)
            || self
                .pending
                .values()
                .any(|pending| pending.instance_id == request.instance_id)
        {
            return Err(BackendError::effect_already_exists(&request.instance_id));
        }
        let spec = WindowsRouting::effect_spec(WindowsRouting::block_frames());
        let config = EffectInstanceConfig {
            instance_id: request.instance_id.clone(),
            effect_id: request.effect_id.clone(),
            module_path: request.module_path.clone(),
            enabled: request.enabled,
            parameters: request.parameters.clone(),
            channel_policy: request.channel_policy,
        };
        let ticket = self.begin_prepare_config(&config, spec)?;
        self.pending.insert(ticket, request);
        Ok(ticket)
    }

    /// Queue preparation for both public nodes and private application-route
    /// processors. Keeping this boundary in one place prevents a restore path
    /// from accidentally reintroducing synchronous model/plugin setup.
    pub(super) fn begin_prepare_config(
        &mut self,
        config: &EffectInstanceConfig,
        spec: pw_graph_effects::AudioSpec,
    ) -> BackendResult<EffectTicket> {
        if config.instance_id.trim().is_empty() || config.effect_id.trim().is_empty() {
            return Err(BackendError::native(
                "effect instance and effect IDs cannot be empty",
            ));
        }
        let physical = self
            .host
            .physical_channels_for_request(
                &config.effect_id,
                config.module_path.as_deref(),
                config.channel_policy,
                Some(spec.channels),
            )
            .map_err(BackendError::native)?;
        if physical != spec.channels {
            return Err(BackendError::unsupported(format!(
                "Windows routing currently requires {} channels, provider negotiated {}",
                spec.channels, physical
            )));
        }
        self.loader
            .begin_prepare(
                &self.host,
                EffectPrepareRequest {
                    effect_id: config.effect_id.clone(),
                    module_path: config.module_path.clone(),
                    spec,
                    parameters: config.parameters.clone(),
                    cancellation: pw_graph_effects::EffectCancellation::default(),
                },
            )
            .map_err(BackendError::native)
    }

    pub(super) fn poll_preparation_events(&mut self) -> Vec<EffectPreparationEvent> {
        self.loader.poll_events()
    }

    pub(super) fn take_pending(&mut self, ticket: EffectTicket) -> Option<EffectCreateRequest> {
        self.pending.remove(&ticket)
    }

    pub(super) fn cancel_effect(&self, ticket: EffectTicket) -> bool {
        self.loader.cancel(ticket)
    }

    pub(super) fn descriptors(&self) -> Vec<EffectDescriptor> {
        self.host.descriptors()
    }

    pub(super) fn instances(&self) -> Vec<EffectInstance> {
        self.iter().cloned().collect()
    }

    pub(super) fn get(&self, instance_id: &str) -> Option<&EffectInstance> {
        self.instances.get(instance_id)
    }

    pub(super) fn has_input_port(&self, input_port: PortId) -> bool {
        self.instances
            .values()
            .any(|instance| instance.input_port == input_port)
    }

    pub(super) fn iter(&self) -> impl Iterator<Item = &EffectInstance> {
        self.instances
            .values()
            .filter(|instance| !self.route_instances.contains(&instance.config.instance_id))
    }

    /// Iterate over both standalone and application-route effects when the
    /// graph is rebuilt. Route effects are intentionally not returned by the
    /// public `instances`/`iter` views above.
    pub(super) fn all_instances(&self) -> impl Iterator<Item = &EffectInstance> {
        self.instances.values()
    }

    pub(super) fn mark_route_instance(&mut self, instance_id: impl Into<String>) {
        self.route_instances.insert(instance_id.into());
    }

    fn remember(&mut self, instance: EffectInstance) {
        self.instances
            .insert(instance.config.instance_id.clone(), instance);
    }

    fn forget(&mut self, instance_id: &str) -> Option<EffectInstance> {
        self.route_instances.remove(instance_id);
        self.instances.remove(instance_id)
    }
}

/// The graph identities an effect instance occupies.
struct EffectIds {
    node: NodeId,
    input: PortId,
    output: PortId,
}

fn effect_ids(instance_id: &str) -> EffectIds {
    EffectIds {
        node: NodeId(graph_id(effect_node_local_id(instance_id))),
        input: PortId(graph_id(effect_input_port_local_id(instance_id))),
        output: PortId(graph_id(effect_output_port_local_id(instance_id))),
    }
}

impl WindowsAudioDriver {
    /// Put an effect's node and its two ports into a graph.
    ///
    /// Used both when the effect is created and after every refresh, because
    /// the worker rebuilds the graph from what Core Audio reports and Core
    /// Audio has never heard of an effect.
    pub(super) fn draw_effect(
        graph: &mut Graph,
        instance: &EffectInstance,
        name: &str,
        position: [f32; 2],
    ) -> BackendResult<()> {
        let node = Node::new(instance.node_id, name.to_owned(), NodeType::Effect).with_serial(
            stable_local_id(&format!("effect:{}", instance.config.instance_id)),
        );
        let mut node = node;
        node.position = position;
        graph.add_node(node)?;
        graph.add_port(Port::new(
            instance.input_port,
            instance.node_id,
            "in",
            Direction::Sink,
            PortType::Audio,
        ))?;
        graph.add_port(Port::new(
            instance.output_port,
            instance.node_id,
            "out",
            Direction::Source,
            PortType::Audio,
        ))?;
        Ok(())
    }

    /// Activate an effect prepared by the shared lifecycle manager. This is
    /// the only creation path used by the live Windows UI; the router receives
    /// an initialized processor and therefore cannot block on model/plugin
    /// setup.
    pub(super) fn create_effect_prepared(
        &mut self,
        request: EffectNodeRequest,
        prepared: PreparedEffect,
    ) -> BackendResult<EffectInstance> {
        if self.effects.get(&request.instance_id).is_some() {
            return Err(BackendError::native(format!(
                "an effect instance already exists as {}",
                request.instance_id
            )));
        }
        if prepared.descriptor.id != request.effect_id {
            return Err(BackendError::native(format!(
                "prepared effect {} does not match requested effect {}",
                prepared.descriptor.id, request.effect_id
            )));
        }
        let expected_spec = WindowsRouting::effect_spec(WindowsRouting::block_frames());
        if prepared.spec != expected_spec {
            return Err(BackendError::native(format!(
                "prepared effect audio spec {:?} does not match Windows route spec {:?}",
                prepared.spec, expected_spec
            )));
        }
        let spec = prepared.spec;
        let descriptor = prepared.descriptor;
        let processor = prepared.processor;
        let ids = effect_ids(&request.instance_id);
        let instance = EffectInstance {
            config: EffectInstanceConfig {
                instance_id: request.instance_id.clone(),
                effect_id: request.effect_id.clone(),
                module_path: request.module_path.clone(),
                enabled: request.enabled,
                parameters: request.parameters.clone(),
                channel_policy: request.channel_policy,
            },
            node_id: ids.node,
            input_port: ids.input,
            output_port: ids.output,
            source: None,
            destination: None,
            error: None,
            diagnostics: None,
            lifecycle: pw_graph_effects::EffectLifecycle::Active,
            health: pw_graph_effects::EffectHealth::Healthy,
        };

        if self.routing.is_none() {
            self.routing = Some(WindowsRouting::start()?);
        }
        let routing = self.routing.as_mut().expect("routing was just started");
        routing.add_prepared_effect(ids.input, ids.output, processor, spec)?;
        if !request.enabled {
            routing.set_effect_bypassed(ids.input, true)?;
        }
        if let Err(error) = Self::draw_effect(
            &mut self.graph,
            &instance,
            &descriptor.name,
            request.position,
        ) {
            let routing = self.routing.as_mut().expect("routing exists");
            let _ = routing.remove_effect(ids.input);
            return Err(error);
        }
        self.positions.insert(ids.node, request.position);
        self.effect_positions
            .insert(request.instance_id.clone(), request.position);
        self.effects.remember(instance.clone());
        Ok(instance)
    }

    /// Activate a prepared effect owned by one persisted application route.
    ///
    /// Route effects use the same realtime host and processor registry as
    /// ordinary effect nodes, but remain private to the route owner so they
    /// cannot be serialized as a second, free-standing effect by the UI.
    pub(super) fn create_application_effect_prepared(
        &mut self,
        config: EffectInstanceConfig,
        position: [f32; 2],
        prepared: PreparedEffect,
    ) -> BackendResult<EffectInstance> {
        let instance = self.create_effect_prepared(
            EffectNodeRequest {
                instance_id: config.instance_id.clone(),
                effect_id: config.effect_id.clone(),
                module_path: config.module_path.clone(),
                enabled: config.enabled,
                parameters: config.parameters.clone(),
                channel_policy: config.channel_policy,
                position,
            },
            prepared,
        )?;
        self.effects.mark_route_instance(config.instance_id);
        Ok(instance)
    }

    /// Transactional insertion for an already-prepared processor. The direct
    /// link is left untouched until node activation has succeeded, and every
    /// subsequent routing failure rolls back to the original endpoints.
    pub(super) fn insert_effect_prepared_into_link(
        &mut self,
        request: EffectInsertRequest,
        prepared: PreparedEffect,
    ) -> BackendResult<EffectInstance> {
        let source = request.source.clone();
        let destination = request.destination.clone();
        let (output, input) = self.resolve_effect_endpoints(&source, &destination)?;
        let direct = self
            .graph
            .links
            .values()
            .find(|link| link.output_port == output && link.input_port == input)
            .map(|link| link.id)
            .ok_or_else(|| BackendError::native("the link to insert into is no longer present"))?;

        let instance_id = request.instance_id.clone();
        let instance = self.create_effect_prepared(request.into(), prepared)?;
        let inserted = (|| {
            self.disconnect(direct)?;
            self.connect(output, instance.input_port)?;
            self.connect(instance.output_port, input)?;
            Ok::<(), BackendError>(())
        })();
        if let Err(error) = inserted {
            let cleanup = self.destroy_effect(&instance_id);
            let restore = if self.graph.link(direct).is_some() {
                Ok(())
            } else {
                self.connect(output, input).map(|_| ())
            };
            let mut also = Vec::new();
            if let Err(restore_error) = restore {
                also.push(format!(
                    "could not restore the original link: {restore_error}"
                ));
            }
            if let Err(cleanup_error) = cleanup {
                also.push(format!(
                    "could not clean up the effect node: {cleanup_error}"
                ));
            }
            if also.is_empty() {
                return Err(error);
            }
            return Err(BackendError::Native(format!(
                "{error}; additionally, {}",
                also.join("; ")
            )));
        }
        let stored = self
            .effects
            .instances
            .get_mut(&instance_id)
            .expect("the instance was just created");
        stored.source = Some(source);
        stored.destination = Some(destination);
        Ok(stored.clone())
    }

    /// Remove an effect, restoring the link it was inserted into.
    pub(super) fn destroy_effect(&mut self, instance_id: &str) -> BackendResult<()> {
        let Some(instance) = self.effects.get(instance_id).cloned() else {
            return Err(BackendError::native(format!(
                "no effect instance is registered as {instance_id}"
            )));
        };
        // Resolve the original endpoints before anything is torn down: an
        // inserted effect whose endpoints have vanished cannot honestly have
        // its routing restored, and finding that out afterwards is too late.
        let restore = match (&instance.source, &instance.destination) {
            (Some(source), Some(destination)) => {
                Some(self.resolve_effect_endpoints(source, destination)?)
            }
            _ => None,
        };

        if let Some(routing) = self.routing.as_mut() {
            let dropped = routing.remove_effect(instance.input_port)?;
            for link in dropped {
                let _ = self.graph.remove_link(link.id);
            }
        }
        if let Some(node) = self.graph.nodes.remove(&instance.node_id) {
            for port in node.ports {
                self.graph.ports.remove(&port);
            }
        }
        self.positions.remove(&instance.node_id);
        self.effect_positions.remove(instance_id);
        self.effects.forget(instance_id);

        if let Some((output, input)) = restore {
            // The effect was standing in for a direct link; put it back.
            self.connect(output, input)?;
        }
        Ok(())
    }

    pub(super) fn set_effect_bypassed(
        &mut self,
        instance_id: &str,
        enabled: bool,
    ) -> BackendResult<()> {
        let input_port = self
            .effects
            .get(instance_id)
            .map(|instance| instance.input_port)
            .ok_or_else(|| {
                BackendError::native(format!("no effect instance is registered as {instance_id}"))
            })?;
        let routing = self
            .routing
            .as_mut()
            .ok_or_else(|| BackendError::native("the Windows audio router is not running"))?;
        routing.set_effect_bypassed(input_port, !enabled)?;
        if let Some(instance) = self.effects.instances.get_mut(instance_id) {
            instance.config.enabled = enabled;
        }
        Ok(())
    }

    pub(super) fn set_effect_value(
        &mut self,
        instance_id: &str,
        parameter: &str,
        value: f32,
    ) -> BackendResult<()> {
        let input_port = self
            .effects
            .get(instance_id)
            .map(|instance| instance.input_port)
            .ok_or_else(|| {
                BackendError::native(format!("no effect instance is registered as {instance_id}"))
            })?;
        let routing = self
            .routing
            .as_mut()
            .ok_or_else(|| BackendError::native("the Windows audio router is not running"))?;
        routing.set_effect_parameter(input_port, parameter, value)?;
        // Remembered so the value survives a restart, and so the UI reads back
        // what it set rather than what the descriptor defaults to.
        if let Some(instance) = self.effects.instances.get_mut(instance_id) {
            instance
                .config
                .parameters
                .insert(parameter.to_owned(), value);
        }
        Ok(())
    }

    /// Turn a pair of stable endpoint keys into live port ids.
    ///
    /// Keys rather than ids because an effect outlives the graph rebuild that
    /// produced the ids: §12.2 restores effects after the links they sit in,
    /// and by then the numbers have changed.
    fn resolve_effect_endpoints(
        &self,
        source: &PortKey,
        destination: &PortKey,
    ) -> BackendResult<(PortId, PortId)> {
        let output = self.graph.resolve_port_key(source).ok_or_else(|| {
            BackendError::native(format!("effect source {} is not present", source.node_name))
        })?;
        let input = self.graph.resolve_port_key(destination).ok_or_else(|| {
            BackendError::native(format!(
                "effect destination {} is not present",
                destination.node_name
            ))
        })?;
        Ok((output, input))
    }
}
